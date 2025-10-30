use std::collections::HashMap;

pub struct Orchestrator;

impl Orchestrator {
    pub async fn build(namespace: &str) {
        let ctx = crate::catalog::session::SessionFactory::new_context().await;
        crate::catalog::session::SessionFactory::register_s3_and_wal(&ctx, namespace).await;

        // Register S3 tables using registry prefixes; fallback to configured output parquet location
        let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
        let mut prefixes: Vec<String> = Vec::new();
        if let Some(entry) = crate::sql::registry::find_entry(&pipeline, namespace).await {
            prefixes.extend(entry.data_prefixes);
        }
        // Fallback: use the configured s3 parquet base for this namespace/pipeline
        let s3_loc = crate::helpers::configuration::Config::get_output_parquet_s3_location(&pipeline).unwrap_or_default();
        if prefixes.is_empty() && !s3_loc.is_empty() { prefixes.push(s3_loc.clone()); }
        // Normalize to s3://bucket/dir/
        let bucket = url::Url::parse(&s3_loc).ok().and_then(|u| u.host_str().map(|s| s.to_string())).unwrap_or_default();
        println!("META: orchestrator ns='{}' prefixes_before_norm={}", namespace, prefixes.len());
        for p in prefixes.iter_mut() {
            if !p.starts_with("s3://") {
                let mut dir = p.trim_matches('/').to_string();
                if !dir.ends_with('/') { dir.push('/'); }
                *p = format!("s3://{}/{}", bucket, dir);
            }
        }
        for (idx, path) in prefixes.iter().enumerate() {
            let tname = format!("{}_s3_{}", namespace, idx);
            let _ = ctx.register_parquet(&tname, path, datafusion::prelude::ParquetReadOptions::default()).await;
        }
        println!("META: orchestrator ns='{}' registered_sources={}", namespace, prefixes.len());

        // Stats → Catalog (+LLM in build_catalog tail)
        // Full S3 scan for authoritative stats (0 => no limit). For now, apply a small limit for fast runs.
        crate::catalog::stats_builder::StatsBuilder::compute_and_write(&ctx, namespace, 10).await.expect("TODO: panic message");
        crate::catalog::catalog::CatalogBuilder::build_and_write(namespace).await;
        // LLM dataset-level description enrichment (couple of lines) - update only top-level description on S3
        if crate::helpers::configuration::Config::pipeline_llm_enabled() && crate::helpers::configuration::Config::catalog_llm_enabled() {
            let llm = crate::llm::create_llm(&crate::llm::config_from_env());
            // Build short context from current semantic (async, S3-backed)
            let semantic = crate::catalog::infer::infer_semantic_model_async(namespace).await;
            if !semantic.fields.is_empty() {
                let mut lines: Vec<String> = Vec::new();
                lines.push(format!("Dataset namespace: {}", namespace));
                lines.push(format!("Fields: {}", semantic.fields.iter().map(|f| f.name.clone()).take(10).collect::<Vec<_>>().join(", ")));
                let prompt = format!("You are documenting a dataset. In 2-3 concise lines, describe what the dataset logically represents, the kind of events/entities involved, and typical use-cases. Keep it neutral and helpful.\n{}", lines.join("\n"));
                // Call LLM via spawn_blocking + timeout; avoid catch_unwind requirements
                let llm_clone = llm.clone();
                let prompt_clone = prompt.clone();
                let res = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    tokio::task::spawn_blocking(move || llm_clone.chat(&[crate::llm::ChatMessage { role: "user".into(), content: prompt_clone }]))
                ).await;
                if let Ok(Ok(Ok(text))) = res {
                    println!("META: LLM catalog description ns='{}' text='{}'", namespace, text);
                    let summary = text.lines().take(3).collect::<Vec<_>>().join(" ");
                    // Fetch existing catalog JSON from S3 and update description only
                    let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
                    if let Some(entry) = crate::sql::registry::find_entry(&pipeline, namespace).await {
                        if !entry.catalog_key.is_empty() {
                            if let Ok(mut v) = crate::helpers::s3::get_json(&entry.catalog_key).await {
                                v.as_object_mut().map(|obj| obj.insert("description".to_string(), serde_json::Value::String(summary.clone())));
                                println!("Catalog contents after LLM update: {}", v);
                                let _ = crate::helpers::s3::put_json(&entry.catalog_key, &v).await;
                            } else {
                                // Fallback: rebuild from semantic and set description
                                let cat = crate::catalog::model::DataCatalog { namespace: namespace.to_string(), description: Some(summary.clone()), dimensions: semantic.dimensions.clone(), metrics: semantic.metrics.clone(), fields: semantic.fields.iter().map(|f| crate::catalog::model::CatalogField { entity: String::new(), name: f.name.clone(), description: None, synonyms: None, pii_sensitivity: None, units_or_format: None, role: Some(format!("{:?}", f.role)) }).collect() };
                                crate::helpers::configuration::Config::write_catalog_async(namespace, &cat).await;
                            }
                        }
                    }
                }
            }
        }
    }

    pub async fn build_all(namespaces: &HashMap<String, crate::discover::Metadata>) {
        // Prefer namespaces from registry to ensure S3-backed list, fallback to metadata keys
        let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
        let mut regs = crate::sql::registry::list_namespaces(&pipeline).await;
        if regs.is_empty() {
            regs = namespaces.keys().cloned().collect();
        }
        for ns in regs { Self::build(&ns).await; }
    }
}


