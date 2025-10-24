use std::collections::HashMap;

pub struct Orchestrator;

impl Orchestrator {
    pub async fn build(namespace: &str) {
        let ctx = crate::catalog::session::SessionFactory::new_context().await;
        crate::catalog::session::SessionFactory::register_s3_and_wal(&ctx, namespace).await;

        // Register S3 tables using manifest prefixes if available
        if let Some(s3_loc) = crate::helpers::configuration::Config::get_output_parquet_s3_location(namespace) {
            let mut s3_paths: Vec<String> = Vec::new();
            if let Some(man) = crate::helpers::configuration::Config::read_manifest(namespace).await {
                if let Some(tables) = man.get("tables").and_then(|t| t.as_object()) {
                    if let Some(nsobj) = tables.get(namespace).and_then(|v| v.as_object()) {
                        if let Some(prefixes) = nsobj.get("prefixes").and_then(|p| p.as_array()) {
                            if let Ok(u) = url::Url::parse(&s3_loc) { if let Some(bucket) = u.host_str() { for pref in prefixes { if let Some(pstr) = pref.as_str() { let mut dir = pstr.trim_matches('/').to_string(); if !dir.ends_with('/') { dir.push('/'); } s3_paths.push(format!("s3://{}/{}", bucket, dir)); } } } }
                        }
                    }
                }
            }
            if s3_paths.is_empty() { s3_paths.push(s3_loc.clone()); }
            for (idx, path) in s3_paths.iter().enumerate() {
                let tname = format!("{}_s3_{}", namespace, idx);
                let _ = ctx.register_parquet(&tname, path, datafusion::prelude::ParquetReadOptions::default()).await;
            }
        }

        // Stats → Semantic → Catalog (+LLM in build_catalog tail)
        let _ = crate::catalog::stats_builder::StatsBuilder::compute_and_write(&ctx, namespace, 10_000).await;
        crate::catalog::semantic::SemanticInfer::infer_and_write(namespace).await;
        crate::catalog::catalog::CatalogBuilder::build_and_write(namespace).await;
        // LLM dataset-level description enrichment (couple of lines)
        if crate::helpers::configuration::Config::pipeline_llm_enabled() && crate::helpers::configuration::Config::catalog_llm_enabled() {
            if let Some(val) = crate::helpers::configuration::Config::read_namespace_stats_async(namespace).await {
                if let Ok(mut stats) = serde_json::from_value::<crate::discover::stats::NamespaceStats>(val) {
                    for (_k, fs) in stats.fields.iter_mut() { fs.finalize(); }
                    let semantic = crate::semantics::infer::infer_semantic_model(namespace);
                    let llm = crate::llm::create_llm(&crate::llm::config_from_env());
                    let mut lines: Vec<String> = Vec::new();
                    lines.push(format!("Dataset namespace: {}", namespace));
                    lines.push(format!("Fields: {}", semantic.fields.iter().map(|f| f.name.clone()).take(10).collect::<Vec<_>>().join(", ")));
                    let prompt = format!("You are documenting a dataset. In 2-3 concise lines, describe what the dataset logically represents, the kind of events/entities involved, and typical use-cases. Keep it neutral and helpful.\n{}", lines.join("\n"));
                    if let Ok(text) = llm.chat(&[crate::llm::ChatMessage { role: "user".into(), content: prompt }]) {
                        // Load current catalog, set top-level description, write back
                        let path = crate::helpers::configuration::Config::get_catalog_local_path(namespace);
                        let mut catalog = if let Ok(s) = std::fs::read_to_string(&path) { serde_yaml::from_str::<crate::semantics::model::DataCatalog>(&s).unwrap_or_else(|_| crate::semantics::model::DataCatalog { namespace: namespace.to_string(), description: None, fields: Vec::new() }) } else { crate::semantics::model::DataCatalog { namespace: namespace.to_string(), description: None, fields: Vec::new() } };
                        if catalog.description.is_none() || catalog.description.as_ref().unwrap().trim().is_empty() {
                            let summary = text.lines().take(3).collect::<Vec<_>>().join(" ");
                            catalog.description = Some(summary);
                            crate::helpers::configuration::Config::write_catalog_async(namespace, &catalog).await;
                        }
                    }
                }
            }
        }
    }

    pub async fn build_all(namespaces: &HashMap<String, crate::discover::Metadata>) {
        for (ns, _md) in namespaces.iter() {
            Self::build(ns).await;
        }
    }
}


