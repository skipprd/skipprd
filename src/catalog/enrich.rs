use crate::catalog::model::{DataCatalog, CatalogField};

/// Run dataset-level and field-level LLM enrichment for a namespace.
pub async fn enrich_namespace_with_llm(namespace: &str) {
    if !(crate::helpers::configuration::Config::pipeline_llm_enabled() && crate::helpers::configuration::Config::catalog_llm_enabled()) {
        return;
    }
    let llm = crate::llm::create_llm(&crate::llm::config_from_env());

    // Load semantic from S3
    let semantic = crate::catalog::infer::infer_semantic_model_async(namespace).await;

    // Dataset-level short description
    if !semantic.fields.is_empty() {
        let mut lines: Vec<String> = Vec::new();
        lines.push(format!("Dataset namespace: {}", namespace));
        lines.push(format!("Fields: {}", semantic.fields.iter().map(|f| f.name.clone()).take(10).collect::<Vec<_>>().join(", ")));
        let prompt = format!("You are documenting a dataset. In 2-3 concise lines, describe what the dataset logically represents, the kind of events/entities involved, and typical use-cases. Keep it neutral and helpful.\n{}", lines.join("\n"));
        let llm_clone = llm.clone();
        let prompt_clone = prompt.clone();
        let timeout_secs = crate::helpers::configuration::Config::catalog_llm_timeout_secs();
        let text_opt = if timeout_secs == 0 {
            match tokio::task::spawn_blocking(move || llm_clone.chat(&[crate::llm::ChatMessage { role: "user".into(), content: prompt_clone }])).await {
                Ok(Ok(t)) => Some(t),
                _ => None,
            }
        } else {
            match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), tokio::task::spawn_blocking(move || llm_clone.chat(&[crate::llm::ChatMessage { role: "user".into(), content: prompt }]))).await {
                Ok(Ok(Ok(t))) => Some(t),
                _ => None,
            }
        };
        if let Some(text) = text_opt {
            let summary = text.lines().take(3).collect::<Vec<_>>().join(" ");
            let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
            if let Some(entry) = crate::sql::registry::find_entry(&pipeline, namespace).await {
                if !entry.catalog_key.is_empty() {
                    if let Ok(mut v) = crate::helpers::s3::get_json(&entry.catalog_key).await {
                        v.as_object_mut().map(|obj| obj.insert("description".to_string(), serde_json::Value::String(summary.clone())));
                        let _ = crate::helpers::s3::put_json(&entry.catalog_key, &v).await;
                    }
                }
            }
        }
    }

    // Field-level enrichment
    let ns_stats: Option<crate::discover::stats::NamespaceStats> = match crate::helpers::configuration::Config::read_namespace_stats_async(namespace).await {
        Some(v) => serde_json::from_value::<crate::discover::stats::NamespaceStats>(v).ok(),
        None => None,
    };
    // Load existing catalog (may be YAML stored as JSON via helper)
    let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
    if let Some(entry) = crate::sql::registry::find_entry(&pipeline, namespace).await {
        if !entry.catalog_key.is_empty() {
            if let Ok(val) = crate::helpers::s3::get_json(&entry.catalog_key).await {
                // Build DataCatalog from existing JSON
                let mut catalog = DataCatalog {
                    namespace: namespace.to_string(),
                    description: val.get("description").and_then(|x| x.as_str()).map(|s| s.to_string()),
                    dimensions: semantic.dimensions.clone(),
                    metrics: semantic.metrics.clone(),
                    fields: val.get("fields").and_then(|x| x.as_array()).map(|arr| {
                        arr.iter().filter_map(|f| {
                            let name = f.get("name").and_then(|x| x.as_str())?.to_string();
                            let desc = f.get("description").and_then(|x| x.as_str()).map(|s| s.to_string());
                            let syns = f.get("synonyms").and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|s| s.as_str().map(|t| t.to_string())).collect::<Vec<String>>());
                            let pii = f.get("pii_sensitivity").and_then(|x| x.as_str()).map(|s| s.to_string());
                            let units = f.get("units_or_format").and_then(|x| x.as_str()).map(|s| s.to_string());
                            Some(CatalogField { entity: String::new(), name, description: desc, synonyms: syns, pii_sensitivity: pii, units_or_format: units, role: None })
                        }).collect::<Vec<CatalogField>>()
                    }).unwrap_or_else(|| semantic.fields.iter().map(|f| CatalogField { entity: String::new(), name: f.name.clone(), description: None, synonyms: None, pii_sensitivity: None, units_or_format: None, role: Some(format!("{:?}", f.role)) }).collect()),
                };
                crate::catalog::writer::enrich_field_descriptions_with_llm(namespace, &semantic, ns_stats.as_ref(), &mut catalog).await;
                crate::helpers::configuration::Config::write_catalog_async(namespace, &catalog).await;
            }
        }
    }
}

/// Enrich all namespaces with LLM at the end of discover.
pub async fn run_llm_enrichment_all(namespaces: &std::collections::HashMap<String, crate::discover::Metadata>) {
    // Prefer namespaces from registry
    let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
    let mut regs = crate::sql::registry::list_namespaces(&pipeline).await;
    if regs.is_empty() { regs = namespaces.keys().cloned().collect(); }
    for ns in regs { enrich_namespace_with_llm(&ns).await; }
}


