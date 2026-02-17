use crate::adapters::storage::StorageAdapter;
use crate::providers::VectorStore;
use serde_json::Value;
use std::sync::Arc;
use tracing::{info, warn};

pub async fn sync_pipeline(
    storage: Arc<dyn crate::adapters::storage::StorageAdapter>,
    keyspace: Arc<dyn crate::providers::Keyspace>,
    scope: &crate::providers::RequestScope,
    llm: Arc<dyn crate::llm::LargeLanguageModel>,
    vector: Arc<dyn crate::providers::VectorStore>,
    _pipeline: &str,
) -> Result<(), String> {
    // Engine-agnostic: discover datasets by listing stored catalogs under this scope.
    // (Catalogs are the discovery/cache layer; no sqlrt registry usage.)
    let catalog_prefix = format!(
        "{}/{}/{}/catalog/",
        scope.tenant, scope.workspace, scope.project_id
    );
    let keys = storage
        .list_prefix(&catalog_prefix)
        .await
        .unwrap_or_default();
    let mut dataset_ids: Vec<String> = Vec::new();
    // Key format: <prefix>/<encoded_dataset_id>.yaml
    for k in keys.iter() {
        if !k.ends_with(".yaml") {
            continue;
        }
        if let Some(rest) = k.strip_prefix(&catalog_prefix) {
            let stem = rest.trim_end_matches(".yaml");
            if !stem.is_empty() {
                dataset_ids.push(percent_decode(stem));
            }
        }
    }
    dataset_ids.sort();
    dataset_ids.dedup();
    info!(
        "Embeddings: starting sync for project_id='{}' (catalogs={})",
        scope.project_id,
        dataset_ids.len()
    );
    if dataset_ids.is_empty() {
        warn!("No catalogs found under prefix '{}'", catalog_prefix);
    }
    let _ = keyspace.clone(); // keyspace is used for DBT artifact scanning below.

    // Build items (datasets + fields) and upsert in manageable batches
    let mut items: Vec<react_core::providers::VectorChunk> = Vec::new();
    let epoch = chrono::Utc::now().timestamp() as u64;
    for ns in dataset_ids.iter() {
        // Catalog
        let mut dataset_text = String::new();
        let mut field_count_ns: usize = 0;
        let catalog_key = keyspace.catalog_key(scope, ns);
        if let Ok(val) = storage.get_json(&catalog_key).await {
            // Dataset description
            if let Some(desc) = val.get("description").and_then(|x| x.as_str()) {
                if !desc.trim().is_empty() {
                    dataset_text.push_str(desc);
                }
            }
            // Governance digest (curated catalog note)
            if let Some(gn) = val
                .get("governance_notes")
                .and_then(|g| g.get("dataset"))
                .and_then(|d| d.get("digest"))
                .and_then(|x| x.as_str())
            {
                if !gn.trim().is_empty() {
                    if !dataset_text.is_empty() {
                        dataset_text.push(' ');
                    }
                    dataset_text.push_str(gn);
                }
            }
            // Fields as separate chunks
            if let Some(fields) = val.get("fields").and_then(|x| x.as_array()) {
                for f in fields {
                    let name = f.get("name").and_then(|x| x.as_str()).unwrap_or("");
                    let role = f.get("role").and_then(|x| x.as_str()).unwrap_or("");
                    let synonyms = f
                        .get("synonyms")
                        .and_then(|x| x.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
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
                        if let Some(d) = distinct {
                            parts.push(format!("distinct≈{}", d));
                        }
                        if let Some(mn) = min_numeric {
                            parts.push(format!("min={}", mn));
                        }
                        if let Some(mx) = max_numeric {
                            parts.push(format!("max={}", mx));
                        }
                        if let Some(ml) = max_len {
                            parts.push(format!("max_len={}", ml));
                        }
                        if nulls > 0 {
                            parts.push(format!("nulls={}", nulls));
                        }
                        if !parts.is_empty() {
                            stats_snip = format!(" [{}]", parts.join(" "));
                        }
                    }
                    let text = format!(
                        "field:{} role:{} syn:{} desc:{}{}",
                        name, role, synonyms, fdesc, stats_snip
                    );
                    items.push(react_core::providers::VectorChunk {
                        id: format!("field:{}:{}", ns, name),
                        kind: "field".to_string(),
                        dataset_id: ns.clone(),
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
        // Stats summary for dataset
        // Use catalog-embedded stats for dataset summary if available
        if let Ok(val) = storage.get_json(&catalog_key).await {
            if let Some(fields) = val.get("fields").and_then(|x| x.as_array()) {
                let mut top: Vec<String> = Vec::new();
                for f in fields.iter() {
                    if let Some(fname) = f.get("name").and_then(|x| x.as_str()) {
                        let st = f.get("stats");
                        let mut parts: Vec<String> = Vec::new();
                        if let Some(d) = st
                            .and_then(|x| x.get("approx_distinct"))
                            .and_then(|x| x.as_u64())
                        {
                            parts.push(format!("distinct≈{}", d));
                        }
                        if let Some(mx) = st.and_then(|x| x.get("max_len")).and_then(|x| x.as_u64())
                        {
                            parts.push(format!("max_len={}", mx));
                        }
                        if !parts.is_empty() {
                            top.push(format!("{} [{}]", fname, parts.join(" ")));
                        }
                        if top.len() >= 12 {
                            break;
                        }
                    }
                }
                if !top.is_empty() {
                    if !dataset_text.is_empty() {
                        dataset_text.push(' ');
                    }
                    dataset_text.push_str(&format!("fields: {}", top.join("; ")));
                }
            }
        }
        if !dataset_text.is_empty() {
            let preview = if dataset_text.len() <= 200 {
                dataset_text.clone()
            } else {
                match dataset_text
                    .char_indices()
                    .take_while(|(i, _)| *i <= 200)
                    .last()
                {
                    Some((i, _)) => dataset_text[..i].to_string(),
                    None => String::new(),
                }
            };
            info!("Embeddings: ns='{}' dataset_text: {}", ns, preview);
            items.push(react_core::providers::VectorChunk {
                id: format!("dataset:{}", ns),
                kind: "dataset".to_string(),
                dataset_id: ns.clone(),
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
            let base = keyspace.dbt_prefix(scope).trim_end_matches('/').to_string();
            // Directory scanners with inferred types
            #[derive(Clone, Copy)]
            struct Scan<'a> {
                dir: &'a str,
                type_hint: &'a str,
            }
            let scans_ns = [
                Scan {
                    dir: "models",
                    type_hint: "dbt_model",
                },
                Scan {
                    dir: "metrics",
                    type_hint: "dbt_metricflow",
                },
                Scan {
                    dir: "macros",
                    type_hint: "dbt_macro",
                },
                Scan {
                    dir: "snapshots",
                    type_hint: "dbt_snapshot",
                },
                Scan {
                    dir: "seeds",
                    type_hint: "dbt_seed",
                },
                Scan {
                    dir: "analyses",
                    type_hint: "dbt_analysis",
                },
                Scan {
                    dir: "tests",
                    type_hint: "dbt_test",
                },
                Scan {
                    dir: "exposures",
                    type_hint: "dbt_exposure",
                },
                Scan {
                    dir: "docs",
                    type_hint: "dbt_doc",
                },
            ];
            for sc in scans_ns.iter() {
                let prefix = format!("{}/{}/{}/", base, sc.dir, ns);
                let keys = storage.list_prefix(&prefix).await.unwrap_or_default();
                for k in keys {
                    if k.contains("/_versions/") {
                        continue;
                    }
                    // fetch content (best-effort; skip very large objects by size hint)
                    // NOTE: StorageAdapter doesn't expose size; accept best-effort for now.
                    if let Ok(bytes) = storage.get_bytes(&k).await {
                        let text = String::from_utf8_lossy(&bytes).to_string();
                        let atype = infer_type_from_key(&k, sc.type_hint);
                        items.push(react_core::providers::VectorChunk {
                            id: format!("artifact:{}:{}:{}", atype, ns, extract_artifact_name(&k)),
                            kind: "artifact".to_string(),
                            dataset_id: ns.clone(),
                            field: None,
                            text,
                            vector: Vec::new(),
                            meta: serde_json::json!({"type": atype, "path": k}),
                            epoch,
                        });
                    }
                }
            }
            // Top-level project files (no dataset_id folder)
            let top_files = [
                ("dbt_project.yml", "dbt_project"),
                ("packages.yml", "dbt_packages"),
            ];
            for (fname, tname) in top_files.iter() {
                let key = format!("{}/{}", base, fname);
                if let Ok(bytes) = storage.get_bytes(&key).await {
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    items.push(react_core::providers::VectorChunk {
                        id: format!("artifact:{}:{}:{}", *tname, ns, fname),
                        kind: "artifact".to_string(),
                        dataset_id: ns.clone(),
                        field: None,
                        text,
                        vector: Vec::new(),
                        meta: serde_json::json!({"type": *tname, "path": key}),
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
        vector.upsert(scope, &items[p..q]).await?;
        info!("Embeddings: upserted {} / {} chunks", q, total);
        p = q;
    }
    info!("Embeddings sync completed: {} items", items.len());
    Ok(())
}

fn extract_artifact_name(key: &str) -> String {
    // expect .../{dataset_id}/{name}.ext
    if let Some(pos) = key.rfind('/') {
        let name_ext = &key[pos + 1..];
        name_ext
            .trim_end_matches(".sql")
            .trim_end_matches(".yaml")
            .to_string()
    } else {
        key.to_string()
    }
}

fn infer_type_from_key(key: &str, default_hint: &str) -> String {
    // Derive type from directory and extension; fallback to provided hint
    let lower = key.to_lowercase();
    let t = if lower.contains("/models/") && lower.ends_with(".sql") {
        "dbt_model"
    } else if lower.contains("/models/") && (lower.ends_with(".yml") || lower.ends_with(".yaml")) {
        "dbt_schema"
    } else if lower.contains("/metrics/") {
        "dbt_metricflow"
    } else if lower.contains("/macros/") {
        "dbt_macro"
    } else if lower.contains("/snapshots/") {
        "dbt_snapshot"
    } else if lower.contains("/seeds/") && (lower.ends_with(".csv") || lower.ends_with(".parquet"))
    {
        "dbt_seed"
    } else if lower.contains("/analyses/") {
        "dbt_analysis"
    } else if lower.contains("/tests/") {
        "dbt_test"
    } else if lower.contains("/exposures/") {
        "dbt_exposure"
    } else if lower.contains("/docs/") {
        "dbt_doc"
    } else if lower.ends_with("/dbt_project.yml") {
        "dbt_project"
    } else if lower.ends_with("/packages.yml") {
        "dbt_packages"
    } else {
        // generic based on extension
        if lower.ends_with(".sql") {
            "file_sql"
        } else if lower.ends_with(".yaml") || lower.ends_with(".yml") {
            "file_yaml"
        } else if lower.ends_with(".md") {
            "file_md"
        } else if lower.ends_with(".py") {
            "file_py"
        } else {
            default_hint
        }
    };
    t.to_string()
}

fn percent_decode(s: &str) -> String {
    // Minimal percent-decoder for key components.
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h1 = bytes[i + 1];
            let h2 = bytes[i + 2];
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(a), Some(b)) = (hex(h1), hex(h2)) {
                out.push((a * 16 + b) as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

// NOTE: Removed sync_all_pipelines legacy entrypoint; call `sync_pipeline(...)` with explicit providers.
