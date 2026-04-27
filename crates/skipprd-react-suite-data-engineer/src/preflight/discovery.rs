use react_core::provider_traits::VectorCollection;
use react_core::storage::{retry_get_json, retry_put_json};

/// Limits for dataset discovery (kept minimal; expand as needed).
#[derive(Clone, Debug, Default)]
pub struct DiscoveryLimits {
    pub top_k_datasets: usize,
}

#[derive(Clone, Debug, Default)]
pub struct DiscoveryBundle {
    /// (dataset_id, score)
    pub datasets: Vec<(String, f32)>,
}

/// Minimal discovery implementation (suite-focused): returns top-k dataset candidates.
///
/// Today we lean on WS/context resolution logic. As we add suite-specific discovery, this can be
/// replaced without changing suite APIs.
pub async fn run_discovery(
    question: &str,
    limits: &DiscoveryLimits,
    sctx: &react_core::suite::SuiteCtx,
) -> DiscoveryBundle {
    let k = if limits.top_k_datasets == 0 {
        12
    } else {
        limits.top_k_datasets
    };
    let vector = match sctx.vector().as_ref() {
        Some(v) => v,
        None => {
            return DiscoveryBundle {
                datasets: Vec::new(),
            }
        }
    };
    let vec = match sctx.llm_embed(&[question.to_string()]) {
        Ok(mut v) => v.pop().unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    if vec.is_empty() {
        return DiscoveryBundle {
            datasets: Vec::new(),
        };
    }

    // Query within the current ReAct project scope.
    let mut all: Vec<(String, f32)> = Vec::new();
    if let Ok(hits) = vector.query(sctx.scope(), &vec, k * 3, None).await {
        for h in hits {
            if h.item.namespace == crate::vector_docs::ManualVectorCollection::NAMESPACE {
                if let Ok(meta) = serde_json::from_str::<crate::vector_docs::ManualVectorMetadata>(
                    &h.item.metadata_json,
                ) {
                    if meta.kind == "dataset" {
                        if let Some(dataset_id) = meta.dataset_id {
                            all.push((dataset_id, h.score));
                        }
                    }
                }
            } else if h.item.namespace == "dataset" {
                if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&h.item.metadata_json) {
                    if let Some(dataset_id) = meta.get("dataset_id").and_then(|v| v.as_str()) {
                        all.push((dataset_id.to_string(), h.score));
                    }
                }
            }
        }
    }
    // Dedupe by dataset_id, keep lowest score
    use std::collections::HashMap;
    let mut best: HashMap<String, f32> = HashMap::new();
    for (ds, s) in all.into_iter() {
        let entry = best.entry(ds).or_insert(s);
        if s < *entry {
            *entry = s;
        }
    }
    let mut pairs: Vec<(String, f32)> = best.into_iter().collect();
    pairs.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let datasets = pairs.into_iter().take(k).collect();
    DiscoveryBundle { datasets }
}

pub async fn run_discovery_cached(
    thread_id: &str,
    question: &str,
    limits: &DiscoveryLimits,
    sctx: &react_core::suite::SuiteCtx,
) -> DiscoveryBundle {
    let k = if limits.top_k_datasets == 0 {
        12
    } else {
        limits.top_k_datasets
    };
    let qhash = react_core::llm_observability::sha256_hex_str(question);
    let root = sctx
        .keyspace()
        .threads_prefix(sctx.scope())
        .trim_end_matches("/threads")
        .trim_end_matches('/')
        .to_string();
    let key = format!(
        "{}/state/{}/discovery_{}_k{}.json",
        root, thread_id, qhash, k
    );

    if let Ok(v) = retry_get_json(sctx.storage().as_ref(), &key).await {
        if let Some(arr) = v.get("datasets").and_then(|x| x.as_array()) {
            let mut out: Vec<(String, f32)> = Vec::new();
            for it in arr {
                let ds = it
                    .get("dataset_id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let score = it.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
                if !ds.is_empty() {
                    out.push((ds, score));
                }
            }
            if !out.is_empty() {
                return DiscoveryBundle { datasets: out };
            }
        }
    }

    let bundle = run_discovery(question, limits, sctx).await;
    let payload = serde_json::json!({
        "thread_id": thread_id,
        "question_sha256": qhash,
        "k": k,
        "datasets": bundle.datasets.iter().map(|(ds, score)| serde_json::json!({
            "dataset_id": ds,
            "score": score
        })).collect::<Vec<_>>(),
        "ts": chrono::Utc::now().to_rfc3339(),
    });
    let _ = retry_put_json(sctx.storage().as_ref(), &key, &payload).await;
    bundle
}
