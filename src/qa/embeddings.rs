use serde_json::Value;
use tracing::{info, warn};

pub async fn sync_pipeline(pipeline: &str) -> Result<(), String> {
    // Discover namespaces from registry
    let mut namespaces = crate::sql::registry::list_namespaces(pipeline).await;
    info!("Embeddings: starting sync for pipeline='{}' (namespaces={})", pipeline, namespaces.len());
    if namespaces.is_empty() {
        warn!("No namespaces found in registry for pipeline '{}'", pipeline);
    }
    let cfg = crate::llm::config_from_env();
    let llm = crate::llm::create_llm(&cfg);
    let store = crate::qa::vector::lance_store::LanceDbStore::new(pipeline);

    // Build items (datasets + fields) and upsert in manageable batches
    let mut items: Vec<crate::qa::vector::lance_store::Chunk> = Vec::new();
    let epoch = chrono::Utc::now().timestamp() as u64;
    for ns in namespaces.iter() {
        // Catalog
        let mut dataset_text = String::new();
        let mut field_count_ns: usize = 0;
        if let Some(entry) = crate::sql::registry::find_entry(pipeline, ns).await {
            if !entry.catalog_key.is_empty() {
                if let Ok(val) = crate::helpers::s3::get_json(&entry.catalog_key).await {
                    // Dataset description
                    if let Some(desc) = val.get("description").and_then(|x| x.as_str()) {
                        if !desc.trim().is_empty() {
                            dataset_text.push_str(desc);
                        }
                    }
                    // Fields as separate chunks
                    if let Some(fields) = val.get("fields").and_then(|x| x.as_array()) {
                        for f in fields {
                            let name = f.get("name").and_then(|x| x.as_str()).unwrap_or("");
                            let role = f.get("role").and_then(|x| x.as_str()).unwrap_or("");
                            let synonyms = f.get("synonyms").and_then(|x| x.as_array()).map(|a| {
                                a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", ")
                            }).unwrap_or_default();
                            let fdesc = f.get("description").and_then(|x| x.as_str()).unwrap_or("");
                            let mut stats_snip = String::new();
                            // Pull stats from catalog-embedded values if present
                            if let Some(st) = f.get("stats") {
                                let distinct = st.get("approx_distinct").and_then(|x| x.as_u64());
                                let min_numeric = st.get("min_numeric").and_then(|x| x.as_f64());
                                let max_numeric = st.get("max_numeric").and_then(|x| x.as_f64());
                                let max_len = st.get("max_len").and_then(|x| x.as_u64());
                                let nulls = st.get("nulls").and_then(|x| x.as_u64()).unwrap_or(0);
                                let mut parts: Vec<String> = Vec::new();
                                if let Some(d) = distinct { parts.push(format!("distinct≈{}", d)); }
                                if let Some(mn) = min_numeric { parts.push(format!("min={}", mn)); }
                                if let Some(mx) = max_numeric { parts.push(format!("max={}", mx)); }
                                if let Some(ml) = max_len { parts.push(format!("max_len={}", ml)); }
                                if nulls > 0 { parts.push(format!("nulls={}", nulls)); }
                                if !parts.is_empty() { stats_snip = format!(" [{}]", parts.join(" ")); }
                            }
                            let text = format!("field:{} role:{} syn:{} desc:{}{}", name, role, synonyms, fdesc, stats_snip);
                            items.push(crate::qa::vector::lance_store::Chunk {
                                id: format!("field:{}:{}:{}", pipeline, ns, name),
                                kind: "field".to_string(),
                                namespace: ns.clone(),
                                field: Some(name.to_string()),
                                text,
                                vector: Vec::new(),
                                meta: Value::Null,
                                epoch,
                            });
                            field_count_ns += 1;
                        }
                    }
                }
            }
        }
        // Stats summary for dataset
        // Use catalog-embedded stats for dataset summary if available
        if let Some(entry) = crate::sql::registry::find_entry(pipeline, ns).await {
            if !entry.catalog_key.is_empty() {
                if let Ok(val) = crate::helpers::s3::get_json(&entry.catalog_key).await {
                    if let Some(fields) = val.get("fields").and_then(|x| x.as_array()) {
                        let mut top: Vec<String> = Vec::new();
                        for f in fields.iter() {
                            if let Some(fname) = f.get("name").and_then(|x| x.as_str()) {
                                let st = f.get("stats");
                                let mut parts: Vec<String> = Vec::new();
                                if let Some(d) = st.and_then(|x| x.get("approx_distinct")).and_then(|x| x.as_u64()) { parts.push(format!("distinct≈{}", d)); }
                                if let Some(mx) = st.and_then(|x| x.get("max_len")).and_then(|x| x.as_u64()) { parts.push(format!("max_len={}", mx)); }
                                if !parts.is_empty() { top.push(format!("{} [{}]", fname, parts.join(" "))); }
                                if top.len() >= 12 { break; }
                            }
                        }
                        if !top.is_empty() {
                            if !dataset_text.is_empty() { dataset_text.push(' '); }
                            dataset_text.push_str(&format!("fields: {}", top.join("; ")));
                        }
                    }
                }
            }
        }
        if !dataset_text.is_empty() {
            let preview = if dataset_text.len() <= 200 {
                dataset_text.clone()
            } else {
                match dataset_text.char_indices().take_while(|(i, _)| *i <= 200).last() {
                    Some((i, _)) => dataset_text[..i].to_string(),
                    None => String::new(),
                }
            };
            info!("Embeddings: ns='{}' dataset_text: {}", ns, preview);
            items.push(crate::qa::vector::lance_store::Chunk {
                id: format!("dataset:{}:{}", pipeline, ns),
                kind: "dataset".to_string(),
                namespace: ns.clone(),
                field: None,
                text: dataset_text,
                vector: Vec::new(),
                meta: Value::Null,
                epoch,
            });
        }
        info!("Embeddings: ns='{}' field_items={}", ns, field_count_ns);
    }
    // Embed in batches
    let batch = 64usize;
    let mut i = 0usize;
    while i < items.len() {
        let j = (i + batch).min(items.len());
        let texts: Vec<String> = items[i..j].iter().map(|c| c.text.clone()).collect();
        let vecs = llm.embed(&texts).map_err(|e| e.to_string())?;
        for (k, v) in vecs.into_iter().enumerate() {
            items[i + k].vector = v;
        }
        i = j;
    }
    // Upsert in chunks
    let mut p = 0usize;
    let total = items.len();
    while p < items.len() {
        let q = (p + batch).min(items.len());
        store.upsert(&items[p..q]).await?;
        info!("Embeddings: upserted {} / {} chunks", q, total);
        p = q;
    }
    info!("Embeddings sync completed: {} items", items.len());
    Ok(())
}

pub async fn sync_all_pipelines() -> Result<(), String> {
    let pipelines = crate::sql::registry::list_pipelines().await;
    if pipelines.is_empty() {
        return Err("Central registry missing or empty. Run discover/sync to build registry.".to_string());
    }
    for p in pipelines {
        let _ = sync_pipeline(&p).await?;
    }
    Ok(())
}


