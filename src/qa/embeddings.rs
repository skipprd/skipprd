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
                    // Governance digest (curated catalog note)
                    if let Some(gn) = val.get("governance_notes").and_then(|g| g.get("dataset")).and_then(|d| d.get("digest")).and_then(|x| x.as_str()) {
                        if !gn.trim().is_empty() {
                            if !dataset_text.is_empty() { dataset_text.push(' '); }
                            dataset_text.push_str(gn);
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

        // Artifacts: index current (non-versioned) DBT project content (rich types)
        {
            let bucket = crate::helpers::configuration::Config::get_skippr_s3_bucket();
            let client = crate::helpers::s3::get_s3_client().await;
            let base = {
                let tenant = crate::helpers::configuration::Config::get_tenant();
                let workspace = crate::helpers::configuration::Config::get_workspace_name();
                format!("{}/{}/{}/dbt", tenant, workspace, pipeline)
            };
            // Directory scanners with inferred types
            #[derive(Clone, Copy)]
            struct Scan<'a> { dir: &'a str, type_hint: &'a str }
            let scans_ns = [
                Scan { dir: "models", type_hint: "dbt_model" },
                Scan { dir: "metrics", type_hint: "dbt_metricflow" },
                Scan { dir: "macros", type_hint: "dbt_macro" },
                Scan { dir: "snapshots", type_hint: "dbt_snapshot" },
                Scan { dir: "seeds", type_hint: "dbt_seed" },
                Scan { dir: "analyses", type_hint: "dbt_analysis" },
                Scan { dir: "tests", type_hint: "dbt_test" },
                Scan { dir: "exposures", type_hint: "dbt_exposure" },
                Scan { dir: "docs", type_hint: "dbt_doc" },
            ];
            for sc in scans_ns.iter() {
                let prefix = format!("{}/{}/{}/", base, sc.dir, ns);
                let mut token: Option<String> = None;
                loop {
                    let mut req = client.list_objects_v2().bucket(&bucket).prefix(&prefix).max_keys(1000);
                    if let Some(t) = token.as_ref() { req = req.continuation_token(t); }
                    match req.send().await {
                        Ok(resp) => {
                            for obj in resp.contents() {
                                if let Some(k) = obj.key() {
                                    if k.contains("/_versions/") { continue; }
                                    // fetch content (best-effort; skip very large objects by size hint)
                                    if let Some(sz) = obj.size() {
                                        if sz > 2_000_000 { continue; } // skip files >2MB
                                    }
                                    if let Ok(bytes) = crate::helpers::s3::get_bytes(k).await {
                                        let text = String::from_utf8_lossy(&bytes).to_string();
                                        let atype = infer_type_from_key(k, sc.type_hint);
                                        items.push(crate::qa::vector::lance_store::Chunk {
                                            id: format!("artifact:{}:{}:{}:{}", atype, pipeline, ns, extract_artifact_name(k)),
                                            kind: "artifact".to_string(),
                                            namespace: ns.clone(),
                                            field: None,
                                            text,
                                            vector: Vec::new(),
                                            meta: serde_json::json!({"type": atype, "path": k, "s3_uri": format!("s3://{}/{}", bucket, k)}),
                                            epoch,
                                        });
                                    }
                                }
                            }
                            if resp.next_continuation_token().is_none() { break; }
                            token = resp.next_continuation_token().map(|s| s.to_string());
                        }
                        Err(_) => break,
                    }
                }
            }
            // Top-level project files (no namespace folder)
            let top_files = [("dbt_project.yml", "dbt_project"), ("packages.yml", "dbt_packages")];
            for (fname, tname) in top_files.iter() {
                let key = format!("{}/{}", base, fname);
                if let Ok(bytes) = crate::helpers::s3::get_bytes(&key).await {
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    items.push(crate::qa::vector::lance_store::Chunk {
                        id: format!("artifact:{}:{}:{}:{}", *tname, pipeline, ns, fname),
                        kind: "artifact".to_string(),
                        namespace: ns.clone(),
                        field: None,
                        text,
                        vector: Vec::new(),
                        meta: serde_json::json!({"type": *tname, "path": key, "s3_uri": format!("s3://{}/{}", bucket, key)}),
                        epoch,
                    });
                }
            }
        }
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

fn extract_artifact_name(key: &str) -> String {
    // expect .../{namespace}/{name}.ext
    if let Some(pos) = key.rfind('/') {
        let name_ext = &key[pos + 1..];
        name_ext.trim_end_matches(".sql").trim_end_matches(".yaml").to_string()
    } else {
        key.to_string()
    }
}

fn infer_type_from_key(key: &str, default_hint: &str) -> String {
    // Derive type from directory and extension; fallback to provided hint
    let lower = key.to_lowercase();
    let t = if lower.contains("/models/") && lower.ends_with(".sql") { "dbt_model" }
        else if lower.contains("/models/") && (lower.ends_with(".yml") || lower.ends_with(".yaml")) { "dbt_schema" }
        else if lower.contains("/metrics/") { "dbt_metricflow" }
        else if lower.contains("/macros/") { "dbt_macro" }
        else if lower.contains("/snapshots/") { "dbt_snapshot" }
        else if lower.contains("/seeds/") && (lower.ends_with(".csv") || lower.ends_with(".parquet")) { "dbt_seed" }
        else if lower.contains("/analyses/") { "dbt_analysis" }
        else if lower.contains("/tests/") { "dbt_test" }
        else if lower.contains("/exposures/") { "dbt_exposure" }
        else if lower.contains("/docs/") { "dbt_doc" }
        else if lower.ends_with("/dbt_project.yml") { "dbt_project" }
        else if lower.ends_with("/packages.yml") { "dbt_packages" }
        else {
            // generic based on extension
            if lower.ends_with(".sql") { "file_sql" }
            else if lower.ends_with(".yaml") || lower.ends_with(".yml") { "file_yaml" }
            else if lower.ends_with(".md") { "file_md" }
            else if lower.ends_with(".py") { "file_py" }
            else { default_hint }
        };
    t.to_string()
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


