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
    sctx: &crate::suites::SuiteCtx,
) -> DiscoveryBundle {
    let k = if limits.top_k_datasets == 0 { 12 } else { limits.top_k_datasets };
    let vector = match sctx.vector.as_ref() {
        Some(v) => v,
        None => return DiscoveryBundle { datasets: Vec::new() },
    };
    let vec = match sctx.llm.embed(&[question.to_string()]) {
        Ok(mut v) => v.pop().unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    if vec.is_empty() {
        return DiscoveryBundle { datasets: Vec::new() };
    }

    // Query within the current ReAct project scope.
    let mut all: Vec<(String, f32)> = Vec::new();
    if let Ok(hits) = vector.query(&sctx.scope, &vec, k, Some("dataset")).await {
        for h in hits {
            all.push((h.item.dataset_id, h.score));
        }
    }
    // Dedupe by dataset_id, keep lowest score
    use std::collections::HashMap;
    let mut best: HashMap<String, f32> = HashMap::new();
    for (ds, s) in all.into_iter() {
        let entry = best.entry(ds).or_insert(s);
        if s < *entry { *entry = s; }
    }
    let mut pairs: Vec<(String, f32)> = best.into_iter().collect();
    pairs.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let datasets = pairs.into_iter().take(k).collect();
    DiscoveryBundle { datasets }
}

