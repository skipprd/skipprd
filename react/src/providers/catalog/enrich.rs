use tracing::debug;
use std::collections::HashMap;
use std::sync::Arc;

use super::types::{CatalogField, DataCatalog};
use crate::llm::ChatMessage;

/// Run dataset-level and field-level LLM enrichment for a namespace.
pub async fn enrich_namespace_with_llm(
    storage: Arc<dyn crate::adapters::storage::StorageAdapter>,
    keyspace: Arc<dyn crate::providers::Keyspace>,
    llm: Arc<dyn crate::llm::LargeLanguageModel>,
    scope: &crate::providers::RequestScope,
    namespace: &str,
    llm_timeout_secs: u64,
    llm_batch_size: usize,
) {

    // Load semantic from S3
    let semantic = super::infer::infer_semantic_model_async(storage.clone(), keyspace.clone(), scope, namespace).await;

    // Dataset-level short description (STRICT JSON)
    if !semantic.fields.is_empty() {
        let mut lines: Vec<String> = Vec::new();
        lines.push(format!("Dataset namespace: {}", namespace));
        lines.push(format!(
            "Fields: {}",
            semantic
                .fields
                .iter()
                .map(|f| f.name.clone())
                .take(10)
                .collect::<Vec<_>>()
                .join(", ")
        ));
        let prompt = format!(
            "Return STRICT JSON only: {{\"description\": \"<≤40 words>\"}}.\\nRules: one or two short sentences; JSON only; start with '{{' and end with '}}'. No labels or prose.\\n\\nContext: \\n+{}\\n\\nOutput JSON:",
            lines.join("\n")
        );
        let prompt_clone = prompt.clone();
        let text_opt = if llm_timeout_secs == 0 {
            let llm0 = llm.clone();
            match tokio::task::spawn_blocking(move || {
                llm0.chat(&[ChatMessage { role: "user".into(), content: prompt_clone }])
            }).await {
                Ok(Ok(t)) => Some(t),
                _ => None,
            }
        } else {
            let llm0 = llm.clone();
            match tokio::time::timeout(
                std::time::Duration::from_secs(llm_timeout_secs),
                tokio::task::spawn_blocking(move || {
                    llm0.chat(&[ChatMessage { role: "user".into(), content: prompt }])
                }),
            )
            .await
            {
                Ok(Ok(Ok(t))) => Some(t),
                _ => None,
            }
        };
        if let Some(text) = text_opt {
            debug!(
                "{} LLM Enrich: dataset description raw for ns='{}': {}",
                chrono::Utc::now().to_rfc3339(),
                namespace,
                text
            );
            let summary_raw = super_extract_json_value(&text)
                .ok()
                .and_then(|v| v.get("description").and_then(|x| x.as_str()).map(|s| s.to_string()))
                .unwrap_or_else(|| {
                    text.lines()
                        .take(2)
                        .collect::<Vec<_>>()
                        .join(" ")
                        .trim_start_matches("Answer:")
                        .trim()
                        .to_string()
                });
            // Guard against placeholder/echoed prompt content (e.g., when LLM backend echoes the prompt)
            let is_placeholder =
                summary_raw.contains('<') || summary_raw.contains('>') || summary_raw.to_lowercase().contains("strict json");
            // Heuristic: ensure short, printable, and not obviously garbage
            let s_trim = summary_raw.trim();
            let ascii_ok = s_trim.chars().all(|c| c.is_ascii() && !c.is_control());
            let len_ok = s_trim.len() <= 220 && s_trim.len() >= 8;
            if !is_placeholder && ascii_ok && len_ok {
                let key = keyspace.catalog_key(scope, namespace);
                if let Ok(mut v) = storage.get_json(&key).await {
                    v.as_object_mut()
                        .map(|obj| obj.insert("description".to_string(), serde_json::Value::String(summary_raw.clone())));
                    let _ = storage.put_json(&key, &v).await;
                }
            }
        }
    }

    // Field-level enrichment
    // Prefer stats embedded in catalog; fallback to separate stats JSON if present
    let ns_stats: Option<crate::discover::stats::DatasetFieldStats> = {
        let key = keyspace.catalog_key(scope, namespace);
        match storage.get_json(&key).await {
            Ok(val) => super::stats_from_catalog::dataset_field_stats_from_catalog_json(namespace, &val),
            Err(_) => None,
        }
    };
    // Load existing catalog (may be YAML stored as JSON via helper)
    let key = keyspace.catalog_key(scope, namespace);
    if let Ok(val) = storage.get_json(&key).await {
                // Build DataCatalog from existing JSON
                let mut catalog = DataCatalog {
                    dataset_id: namespace.to_string(),
                    catalog: val.get("catalog").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    database: val.get("database").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    table: val.get("table").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    description: val.get("description").and_then(|x| x.as_str()).map(|s| s.to_string()),
                    dimensions: semantic.dimensions.clone(),
                    metrics: semantic.metrics.clone(),
                    fields: val
                        .get("fields")
                        .and_then(|x| x.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|f| {
                                    let name = f.get("name").and_then(|x| x.as_str())?.to_string();
                                    let desc = f.get("description").and_then(|x| x.as_str()).map(|s| s.to_string());
                                    let syns = f.get("synonyms").and_then(|x| x.as_array()).map(|a| {
                                        a.iter()
                                            .filter_map(|s| s.as_str().map(|t| t.to_string()))
                                            .collect::<Vec<String>>()
                                    });
                                    let pii = f.get("pii_sensitivity").and_then(|x| x.as_str()).map(|s| s.to_string());
                                    let units = f.get("units_or_format").and_then(|x| x.as_str()).map(|s| s.to_string());
                                    let role = f.get("role").and_then(|x| x.as_str()).map(|s| s.to_string());
                                    // Preserve pre-existing stats if present
                                    let stats_opt = f.get("stats").cloned();
                                    let stats_lite =
                                        stats_opt.and_then(|st| serde_json::from_value::<super::types::FieldStatsLite>(st).ok());
                                    Some(CatalogField {
                                        entity: String::new(),
                                        name,
                                        data_type: f.get("type").and_then(|x| x.as_str()).map(|s| s.to_string()),
                                        description: desc,
                                        synonyms: syns,
                                        pii_sensitivity: pii,
                                        units_or_format: units,
                                        role,
                                        stats: stats_lite,
                                    })
                                })
                                .collect::<Vec<CatalogField>>()
                        })
                        .unwrap_or_else(|| {
                            semantic
                                .fields
                                .iter()
                                .map(|f| CatalogField {
                                    entity: String::new(),
                                    name: f.name.clone(),
                                    data_type: None,
                                    description: None,
                                    synonyms: None,
                                    pii_sensitivity: None,
                                    units_or_format: None,
                                    role: Some(format!("{:?}", f.role)),
                                    stats: None,
                                })
                                .collect()
                        }),
                    // Keep structure_index and dataset_stats if present
                    structure_index: val
                        .get("structure_index")
                        .and_then(|x| x.as_object())
                        .map(|m| {
                            let mut out = std::collections::HashMap::<String, Vec<String>>::new();
                            for (k, v) in m {
                                if let Some(arr) = v.as_array() {
                                    out.insert(
                                        k.clone(),
                                        arr.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect(),
                                    );
                                }
                            }
                            out
                        })
                        .unwrap_or_default(),
                    dataset_stats: val
                        .get("dataset_stats")
                        .and_then(|x| serde_json::from_value::<super::types::DatasetStats>(x.clone()).ok()),
                };
                // Batched field enrichment
                let field_names: Vec<String> = catalog.fields.iter().map(|f| f.name.clone()).collect();
                let batch_size = llm_batch_size;

                // Helper to chunk field list
                let chunks: Vec<Vec<String>> = field_names.chunks(batch_size.max(1)).map(|c| c.to_vec()).collect();

                let mut desc_map_all = std::collections::HashMap::<String, String>::new();
                let mut syn_map_all = std::collections::HashMap::<String, Vec<String>>::new();
                let mut pii_map_all = std::collections::HashMap::<String, String>::new();
                let mut units_map_all = std::collections::HashMap::<String, String>::new();

                // Build role and stats context lookups
                let mut role_by_field = std::collections::HashMap::<String, String>::new();
                for f in semantic.fields.iter() {
                    role_by_field.insert(f.name.clone(), format!("{:?}", f.role));
                }
                let mut stats_snip_by_field = std::collections::HashMap::<String, String>::new();
                if let Some(ns) = ns_stats.as_ref() {
                    for (name, s) in ns.fields.iter() {
                        let mut parts: Vec<String> = Vec::new();
                        if let Some(d) = s.approx_distinct {
                            parts.push(format!("distinct≈{}", d));
                        }
                        if let Some(mn) = s.min_numeric {
                            parts.push(format!("min={}", mn));
                        }
                        if let Some(mx) = s.max_numeric {
                            parts.push(format!("max={}", mx));
                        }
                        if let Some(ml) = s.max_len {
                            parts.push(format!("max_len={}", ml));
                        }
                        parts.push(format!("nulls={}", s.nulls));
                        stats_snip_by_field.insert(name.clone(), parts.join(" "));
                    }
                }

                use std::collections::HashSet;
                let allowed_fields: HashSet<String> = field_names.iter().cloned().collect();

                for fields in chunks.into_iter() {
                    let field_names_json = format!(
                        "[{}]",
                        fields
                            .iter()
                            .map(|n| format!("\"{}\"", n))
                            .collect::<Vec<String>>()
                            .join(",")
                    );
                    // Descriptions batch
                    let items = fields
                        .iter()
                        .map(|n| {
                            format!(
                                "{{name: {}, role: {}, stats: {}}}",
                                n,
                                role_by_field.get(n).cloned().unwrap_or_else(|| "\"Unknown\"".to_string()),
                                serde_json::to_string(stats_snip_by_field.get(n).unwrap_or(&"".to_string()))
                                    .unwrap_or_else(|_| "\"\"".to_string())
                            )
                        })
                        .collect::<Vec<String>>()
                        .join(", ");
                    let prompt_desc = format!(
                        "Return STRICT JSON only: {{\"descriptionByField\": {{ \"<field_name>\": \"<≤20 words>\" }} }}.\nRules: one sentence (≤20 words) per field; JSON only; start with '{{' and end with '}}'. Keys MUST be exactly the provided FieldNames.\n\nDataset: {ns}\nFieldNames: {fnames}\nField details: [{items}]\n\nOutput JSON:",
                        ns = namespace,
                        fnames = field_names_json,
                        items = items
                    );
                    let desc_text = tokio::task::spawn_blocking({
                        let llm0 = llm.clone();
                        let p = prompt_desc.clone();
                        move || llm0.chat(&[ChatMessage { role: "user".into(), content: p }])
                    })
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .unwrap_or_default();
                    debug!(
                        "{} LLM Enrich: description batch raw ns='{}' fields=[{}]: {}",
                        chrono::Utc::now().to_rfc3339(),
                        namespace,
                        fields.join(","),
                        desc_text
                    );
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&desc_text).or_else(|_| super_extract_json_value(&desc_text)) {
                        if let Some(map) = v.get("descriptionByField").and_then(|m| m.as_object()) {
                            for (k, val) in map {
                                if !allowed_fields.contains(k) {
                                    continue;
                                }
                                if let Some(s) = val.as_str() {
                                    let s2 = s.trim();
                                    let is_placeholder =
                                        s2.contains('<') || s2.contains('>') || s2.to_lowercase().contains("strict json");
                                    if !s2.is_empty() && !is_placeholder {
                                        desc_map_all.insert(k.clone(), s2.to_string());
                                    }
                                }
                            }
                        }
                    }
                    // Per-field fallback for any missing keys
                    let mut missing: Vec<String> = fields.iter().filter(|n| !desc_map_all.contains_key(*n)).cloned().collect();
                    if !missing.is_empty() {
                        for fname in missing.drain(..) {
                            let item = format!(
                                "{{name: {}, role: {}, stats: {}}}",
                                fname,
                                role_by_field.get(&fname).cloned().unwrap_or_else(|| "\"Unknown\"".to_string()),
                                serde_json::to_string(stats_snip_by_field.get(&fname).unwrap_or(&"".to_string()))
                                    .unwrap_or_else(|_| "\"\"".to_string())
                            );
                            let single_prompt = format!(
                                "Return STRICT JSON only: {{\"descriptionByField\": {{ \"{fname}\": \"<≤20 words>\" }} }}.\nRules: one sentence (≤20 words) per field; JSON only; start with '{{' and end with '}}'.\nDataset: {ns}\nField details: [{item}]\n\nOutput JSON:",
                                fname = fname,
                                ns = namespace,
                                item = item
                            );
                            let single_text = tokio::task::spawn_blocking({
                                let llm0 = llm.clone();
                                let p = single_prompt.clone();
                                move || llm0.chat(&[ChatMessage { role: "user".into(), content: p }])
                            })
                            .await
                            .ok()
                            .and_then(|r| r.ok())
                            .unwrap_or_default();
                            if let Ok(vs) =
                                serde_json::from_str::<serde_json::Value>(&single_text).or_else(|_| super_extract_json_value(&single_text))
                            {
                                if let Some(m) = vs.get("descriptionByField").and_then(|m| m.as_object()) {
                                    if let Some(val) = m.get(&fname).and_then(|x| x.as_str()) {
                                        let t = val.trim();
                                        if !t.is_empty() {
                                            desc_map_all.insert(fname.clone(), t.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Synonyms batch
                    let items2 = fields.iter().map(|n| format!("{{name: {}}}", n)).collect::<Vec<String>>().join(", ");
                    let prompt_syn = format!(
                        "Return STRICT JSON only: {{\"synonymsByField\": {{ \"<field_name>\": [\"a\",\"b\"] }} }}.\nRules: 3–6 single-word synonyms, lowercase; JSON only; start with '{{' and end with '}}'. Keys MUST match FieldNames.\n\nDataset: {ns}\nFieldNames: {fnames}\nFields: [{items}]\n\nOutput JSON:",
                        ns = namespace,
                        fnames = field_names_json,
                        items = items2
                    );
                    let syn_text = tokio::task::spawn_blocking({
                        let llm0 = llm.clone();
                        let p = prompt_syn.clone();
                        move || llm0.chat(&[ChatMessage { role: "user".into(), content: p }])
                    })
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .unwrap_or_default();
                    debug!(
                        "{} LLM Enrich: synonyms batch raw ns='{}' fields=[{}]: {}",
                        chrono::Utc::now().to_rfc3339(),
                        namespace,
                        fields.join(","),
                        syn_text
                    );
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&syn_text).or_else(|_| super_extract_json_value(&syn_text)) {
                        if let Some(map) = v.get("synonymsByField").and_then(|m| m.as_object()) {
                            for (k, val) in map {
                                if !allowed_fields.contains(k) {
                                    continue;
                                }
                                if let Some(arr) = val.as_array() {
                                    let mut out = Vec::new();
                                    for x in arr {
                                        if let Some(s) = x.as_str() {
                                            let t = s.trim().to_lowercase();
                                            let is_placeholder =
                                                t.contains('<') || t.contains('>') || t.contains("strict json");
                                            if !t.is_empty() && !is_placeholder {
                                                out.push(t);
                                            }
                                        }
                                    }
                                    out.dedup();
                                    if !out.is_empty() {
                                        syn_map_all.insert(k.clone(), out);
                                    }
                                }
                            }
                        }
                    }

                    // PII/Units batch
                    let items3 = fields.iter().map(|n| format!("{{name: {}}}", n)).collect::<Vec<String>>().join(", ");
                    let prompt_pu = format!(
                        "Return STRICT JSON only: {{\"piiUnitsByField\": {{ \"<field_name>\": {{\"pii\": \"none|low|medium|high\", \"units\": \"<units or format>\"}} }} }}.\nRules: units may be null if not applicable; JSON only; start with '{{' and end with '}}'. Keys MUST match FieldNames.\n\nDataset: {ns}\nFieldNames: {fnames}\nFields: [{items}]\n\nOutput JSON:",
                        ns = namespace,
                        fnames = field_names_json,
                        items = items3
                    );
                    let pu_text = tokio::task::spawn_blocking({
                        let llm0 = llm.clone();
                        let p = prompt_pu.clone();
                        move || llm0.chat(&[ChatMessage { role: "user".into(), content: p }])
                    })
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .unwrap_or_default();
                    debug!(
                        "{} LLM Enrich: pii/units batch raw ns='{}' fields=[{}]: {}",
                        chrono::Utc::now().to_rfc3339(),
                        namespace,
                        fields.join(","),
                        pu_text
                    );
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pu_text).or_else(|_| super_extract_json_value(&pu_text)) {
                        if let Some(map) = v.get("piiUnitsByField").and_then(|m| m.as_object()) {
                            for (k, val) in map {
                                if !allowed_fields.contains(k) {
                                    continue;
                                }
                                if let Some(obj) = val.as_object() {
                                    if let Some(p) = obj.get("pii").and_then(|x| x.as_str()) {
                                        let p_l = p.to_lowercase();
                                        if matches!(p_l.as_str(), "none" | "low" | "medium" | "high") {
                                            pii_map_all.insert(k.clone(), p_l);
                                        }
                                    }
                                    if let Some(u) = obj.get("units").and_then(|x| x.as_str()) {
                                        let t = u.trim();
                                        let is_placeholder =
                                            t.contains('<') || t.contains('>') || t.to_lowercase().contains("strict json");
                                        if !t.is_empty() && !is_placeholder {
                                            units_map_all.insert(k.clone(), t.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Apply to catalog
                for fld in catalog.fields.iter_mut() {
                    if fld.description.is_none() {
                        if let Some(s) = desc_map_all.get(&fld.name) {
                            if !s.is_empty() {
                                fld.description = Some(s.clone());
                            }
                        }
                    }
                    if fld.synonyms.is_none() {
                        if let Some(v) = syn_map_all.get(&fld.name) {
                            if !v.is_empty() {
                                fld.synonyms = Some(v.clone());
                            }
                        }
                    }
                    if fld.pii_sensitivity.is_none() {
                        if let Some(p) = pii_map_all.get(&fld.name) {
                            fld.pii_sensitivity = Some(p.clone());
                        }
                    }
                    if fld.units_or_format.is_none() {
                        if let Some(u) = units_map_all.get(&fld.name) {
                            if !u.is_empty() {
                                fld.units_or_format = Some(u.clone());
                            }
                        }
                    }
                    // Ensure role is always populated from semantic if missing in prior catalog
                    if fld.role.is_none() {
                        if let Some(r) = role_by_field.get(&fld.name) {
                            fld.role = Some(r.clone());
                        }
                    }
                }

                // Persist enriched catalog (legacy format: YAML->json equiv)
                let yaml = serde_yaml::to_string(&catalog).unwrap_or_else(|_| "".to_string());
                let value = serde_yaml::from_str::<serde_yaml::Value>(&yaml).unwrap_or(serde_yaml::Value::Null);
                let json_equiv = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
                let _ = storage.put_json(&key, &json_equiv).await;
    }
}

/// Enrich all namespaces with LLM at the end of discover.
pub async fn run_llm_enrichment_all(
    storage: Arc<dyn crate::adapters::storage::StorageAdapter>,
    keyspace: Arc<dyn crate::providers::Keyspace>,
    llm: Arc<dyn crate::llm::LargeLanguageModel>,
    scope: &crate::providers::RequestScope,
    namespaces: &HashMap<String, crate::discover::Metadata>,
    llm_timeout_secs: u64,
    llm_batch_size: usize,
) {
    // Engine-agnostic: iterate provided namespace keys (no sqlrt/registry coupling).
    for ns in namespaces.keys() {
        enrich_namespace_with_llm(
            storage.clone(),
            keyspace.clone(),
            llm.clone(),
            scope,
            ns,
            llm_timeout_secs,
            llm_batch_size,
        ).await;
    }
}

// NOTE: legacy wrapper removed. Call `run_llm_enrichment_all(storage, keyspace, llm, scope, namespaces)` instead.

// Helper to salvage first JSON object from a text block
pub fn super_extract_json_value(text: &str) -> Result<serde_json::Value, serde_json::Error> {
    // Try direct parse first
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
        return Ok(v);
    }
    // Scan for the first balanced JSON object
    let bytes = text.as_bytes();
    let mut depth: i32 = 0;
    let mut start: Option<usize> = None;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'{' {
            if depth == 0 {
                start = Some(i);
            }
            depth += 1;
        } else if *b == b'}' {
            if depth > 0 {
                depth -= 1;
            }
            if depth == 0 {
                if let Some(s) = start {
                    let slice = &text[s..=i];
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(slice) {
                        return Ok(v);
                    }
                }
            }
        }
    }
    serde_json::from_str::<serde_json::Value>(text) // return last error
}

