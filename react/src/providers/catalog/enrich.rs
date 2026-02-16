use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;

use super::types::{CatalogField, DataCatalog};
use crate::llm::ChatMessage;

const GLOBAL_CONTEXT_MIN_CONFIDENCE: f32 = 0.80;

fn global_semantic_key(
    keyspace: &Arc<dyn crate::providers::Keyspace>,
    scope: &crate::providers::RequestScope,
) -> String {
    keyspace.semantic_key(scope, react_core::providers::catalog::types::GLOBAL_SEMANTIC_DATASET_ID)
}

async fn read_global_semantic_context(
    storage: &Arc<dyn crate::adapters::storage::StorageAdapter>,
    keyspace: &Arc<dyn crate::providers::Keyspace>,
    scope: &crate::providers::RequestScope,
) -> Option<react_core::providers::catalog::types::GlobalSemanticContext> {
    let key = global_semantic_key(keyspace, scope);
    let v = storage.get_json(&key).await.ok()?;
    serde_json::from_value::<react_core::providers::catalog::types::GlobalSemanticContext>(v).ok()
}

async fn write_global_semantic_context(
    storage: &Arc<dyn crate::adapters::storage::StorageAdapter>,
    keyspace: &Arc<dyn crate::providers::Keyspace>,
    scope: &crate::providers::RequestScope,
    ctx: &react_core::providers::catalog::types::GlobalSemanticContext,
) {
    let key = global_semantic_key(keyspace, scope);
    let yaml = serde_yaml::to_string(ctx).unwrap_or_else(|_| "".to_string());
    let value = serde_yaml::from_str::<serde_yaml::Value>(&yaml).unwrap_or(serde_yaml::Value::Null);
    let json_equiv = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
    let _ = storage.put_json(&key, &json_equiv).await;
}

fn clamp_and_filter_global_context(
    mut ctx: react_core::providers::catalog::types::GlobalSemanticContext,
) -> react_core::providers::catalog::types::GlobalSemanticContext {
    // Confidence gating (authoritative only).
    ctx.audiences
        .retain(|a| a.confidence >= GLOBAL_CONTEXT_MIN_CONFIDENCE && !a.audience.trim().is_empty());
    ctx.context_bullets.retain(|b| {
        b.confidence >= GLOBAL_CONTEXT_MIN_CONFIDENCE && !b.text.trim().is_empty()
    });
    ctx.dataset_groups.retain(|g| {
        g.confidence >= GLOBAL_CONTEXT_MIN_CONFIDENCE
            && !g.group_name.trim().is_empty()
            && !g.dataset_ids.is_empty()
    });
    ctx.assumptions_and_gaps.retain(|a| {
        a.confidence >= GLOBAL_CONTEXT_MIN_CONFIDENCE && !a.text.trim().is_empty()
    });

    // Bound sizes.
    ctx.audiences.truncate(12);
    ctx.context_bullets.truncate(24);
    ctx.dataset_groups.truncate(40);
    ctx.assumptions_and_gaps.truncate(24);

    // Normalize simple dedup.
    let mut seen = std::collections::HashSet::<String>::new();
    ctx.audiences.retain(|a| seen.insert(a.audience.trim().to_string()));
    let mut seen = std::collections::HashSet::<String>::new();
    ctx.context_bullets
        .retain(|b| seen.insert(b.text.trim().to_string()));
    let mut seen = std::collections::HashSet::<String>::new();
    ctx.dataset_groups
        .retain(|g| seen.insert(g.group_name.trim().to_string()));
    let mut seen = std::collections::HashSet::<String>::new();
    ctx.assumptions_and_gaps
        .retain(|a| seen.insert(a.text.trim().to_string()));

    ctx
}

fn compact_catalog_for_global_context(
    dataset_id: &str,
    cat_json: &serde_json::Value,
) -> serde_json::Value {
    let desc = cat_json
        .get("description")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let approx_rows = cat_json
        .get("dataset_stats")
        .and_then(|x| x.get("approx_total_rows"))
        .and_then(|x| x.as_u64());
    let fields = cat_json
        .get("fields")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    // Keep bounded: take first N fields (catalog is already flattened; nested paths can be large).
    let max_fields = 40usize;
    let mut out_fields: Vec<serde_json::Value> = Vec::new();
    for f in fields.into_iter().take(max_fields) {
        let name = f.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if name.trim().is_empty() {
            continue;
        }
        let ty = f.get("type").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let role = f.get("role").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let stats = f.get("stats").cloned().unwrap_or(serde_json::Value::Null);
        // Stats can be heavy; keep only a small subset if present.
        let stats = if let Some(obj) = stats.as_object() {
            serde_json::json!({
                "total": obj.get("total"),
                "nulls": obj.get("nulls"),
                "approx_distinct": obj.get("approx_distinct"),
                "min_numeric": obj.get("min_numeric"),
                "max_numeric": obj.get("max_numeric"),
                "max_len": obj.get("max_len"),
            })
        } else {
            serde_json::Value::Null
        };
        out_fields.push(serde_json::json!({
            "name": name,
            "type": ty,
            "role": role,
            "stats": stats
        }));
    }
    serde_json::json!({
        "dataset_id": dataset_id,
        "description": desc,
        "approx_total_rows": approx_rows,
        "fields": out_fields
    })
}

/// Run dataset-level and field-level LLM enrichment for a dataset_id.
pub async fn enrich_dataset_with_llm(
    storage: Arc<dyn crate::adapters::storage::StorageAdapter>,
    keyspace: Arc<dyn crate::providers::Keyspace>,
    llm: Arc<dyn crate::llm::LargeLanguageModel>,
    scope: &crate::providers::RequestScope,
    dataset_id: &str,
    llm_timeout_secs: u64,
    llm_batch_size: usize,
) {
    // Load semantic from S3
    let semantic = super::infer::infer_semantic_model_async(
        storage.clone(),
        keyspace.clone(),
        scope,
        dataset_id,
    )
    .await;

    // Dataset-level short description (STRICT JSON)
    if !semantic.fields.is_empty() {
        let mut lines: Vec<String> = Vec::new();
        lines.push(format!("Dataset: {}", dataset_id));
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
                llm0.chat(
                    &[ChatMessage {
                        role: "user".into(),
                        content: prompt_clone,
                    }],
                    &react_core::llm::LlmCallOptions {
                        prompt_id: "react.catalog.enrich.dataset_description",
                        thread_id: None,
                        expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                        max_output_tokens: None,
                        temperature: None,
                        top_p: None,
                        reasoning_effort: None,
                    },
                )
            })
            .await
            {
                Ok(Ok(t)) => Some(t),
                _ => None,
            }
        } else {
            let llm0 = llm.clone();
            match tokio::time::timeout(
                std::time::Duration::from_secs(llm_timeout_secs),
                tokio::task::spawn_blocking(move || {
                    llm0.chat(
                        &[ChatMessage {
                            role: "user".into(),
                            content: prompt,
                        }],
                        &react_core::llm::LlmCallOptions {
                            prompt_id: "react.catalog.enrich.dataset_description",
                            thread_id: None,
                            expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                            max_output_tokens: None,
                            temperature: None,
                            top_p: None,
                            reasoning_effort: None,
                        },
                    )
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
                "{} LLM Enrich: dataset description raw for dataset_id='{}': {}",
                chrono::Utc::now().to_rfc3339(),
                dataset_id,
                text
            );
            let summary_raw = super_extract_json_value(&text)
                .ok()
                .and_then(|v| {
                    v.get("description")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string())
                })
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
            let is_placeholder = summary_raw.contains('<')
                || summary_raw.contains('>')
                || summary_raw.to_lowercase().contains("strict json");
            // Heuristic: ensure short, printable, and not obviously garbage
            let s_trim = summary_raw.trim();
            let ascii_ok = s_trim.chars().all(|c| c.is_ascii() && !c.is_control());
            let len_ok = s_trim.len() <= 220 && s_trim.len() >= 8;
            if !is_placeholder && ascii_ok && len_ok {
                let key = keyspace.catalog_key(scope, dataset_id);
                if let Ok(mut v) = storage.get_json(&key).await {
                    // Do NOT overwrite an existing human-edited description.
                    let mut should_write = true;
                    if let Some(obj) = v.as_object() {
                        if obj
                            .get("description")
                            .and_then(|x| x.as_str())
                            .map(|s| !s.trim().is_empty())
                            .unwrap_or(false)
                        {
                            should_write = false;
                        }
                    }
                    if should_write {
                        v.as_object_mut().map(|obj| {
                            obj.insert(
                                "description".to_string(),
                                serde_json::Value::String(summary_raw.clone()),
                            )
                        });
                        let _ = storage.put_json(&key, &v).await;
                    }
                }
            }
        }
    }

    // Field-level enrichment
    // Prefer stats embedded in catalog; fallback to separate stats JSON if present
    let ns_stats: Option<crate::discover::stats::DatasetFieldStats> = {
        let key = keyspace.catalog_key(scope, dataset_id);
        match storage.get_json(&key).await {
            Ok(val) => {
                super::stats_from_catalog::dataset_field_stats_from_catalog_json(dataset_id, &val)
            }
            Err(_) => None,
        }
    };
    // Load existing catalog (may be YAML stored as JSON via helper)
    let key = keyspace.catalog_key(scope, dataset_id);
    if let Ok(val) = storage.get_json(&key).await {
        // Build DataCatalog from existing JSON
        let mut catalog = DataCatalog {
            dataset_id: dataset_id.to_string(),
            catalog: val
                .get("catalog")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            database: val
                .get("database")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            table: val
                .get("table")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            description: val
                .get("description")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            dimensions: semantic.dimensions.clone(),
            metrics: semantic.metrics.clone(),
            fields: val
                .get("fields")
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|f| {
                            let name = f.get("name").and_then(|x| x.as_str())?.to_string();
                            let desc = f
                                .get("description")
                                .and_then(|x| x.as_str())
                                .map(|s| s.to_string());
                            let syns = f.get("synonyms").and_then(|x| x.as_array()).map(|a| {
                                a.iter()
                                    .filter_map(|s| s.as_str().map(|t| t.to_string()))
                                    .collect::<Vec<String>>()
                            });
                            let pii = f
                                .get("pii_sensitivity")
                                .and_then(|x| x.as_str())
                                .map(|s| s.to_string());
                            let units = f
                                .get("units_or_format")
                                .and_then(|x| x.as_str())
                                .map(|s| s.to_string());
                            let role = f
                                .get("role")
                                .and_then(|x| x.as_str())
                                .map(|s| s.to_string());
                            // Preserve pre-existing stats if present
                            let stats_opt = f.get("stats").cloned();
                            let stats_lite = stats_opt.and_then(|st| {
                                serde_json::from_value::<super::types::FieldStatsLite>(st).ok()
                            });
                            Some(CatalogField {
                                entity: String::new(),
                                name,
                                data_type: f
                                    .get("type")
                                    .and_then(|x| x.as_str())
                                    .map(|s| s.to_string()),
                                root_column: f
                                    .get("root_column")
                                    .and_then(|x| x.as_str())
                                    .map(|s| s.to_string()),
                                field_path: f
                                    .get("field_path")
                                    .and_then(|x| x.as_str())
                                    .map(|s| s.to_string()),
                                structure_kind: f.get("structure_kind").and_then(|x| {
                                    serde_json::from_value::<super::types::StructureKind>(x.clone())
                                        .ok()
                                }),
                                access_descriptor: f.get("access_descriptor").and_then(|x| {
                                    serde_json::from_value::<super::types::AccessDescriptor>(
                                        x.clone(),
                                    )
                                    .ok()
                                }),
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
                            root_column: None,
                            field_path: None,
                            structure_kind: None,
                            access_descriptor: None,
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
                                arr.iter()
                                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                    .collect(),
                            );
                        }
                    }
                    out
                })
                .unwrap_or_default(),
            dataset_stats: val
                .get("dataset_stats")
                .and_then(|x| serde_json::from_value::<super::types::DatasetStats>(x.clone()).ok()),
            built_at_epoch_secs: val.get("built_at_epoch_secs").and_then(|x| x.as_u64()),
        };
        // Batched field enrichment
        let field_names: Vec<String> = catalog.fields.iter().map(|f| f.name.clone()).collect();
        let batch_size = llm_batch_size;

        // Helper to chunk field list
        let chunks: Vec<Vec<String>> = field_names
            .chunks(batch_size.max(1))
            .map(|c| c.to_vec())
            .collect();

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
                        role_by_field
                            .get(n)
                            .cloned()
                            .unwrap_or_else(|| "\"Unknown\"".to_string()),
                        serde_json::to_string(
                            stats_snip_by_field.get(n).unwrap_or(&"".to_string())
                        )
                        .unwrap_or_else(|_| "\"\"".to_string())
                    )
                })
                .collect::<Vec<String>>()
                .join(", ");
            let prompt_desc = format!(
                        "Return STRICT JSON only: {{\"descriptionByField\": {{ \"<field_name>\": \"<≤20 words>\" }} }}.\nRules: one sentence (≤20 words) per field; JSON only; start with '{{' and end with '}}'. Keys MUST be exactly the provided FieldNames.\n\nDataset: {ns}\nFieldNames: {fnames}\nField details: [{items}]\n\nOutput JSON:",
                        ns = dataset_id,
                        fnames = field_names_json,
                        items = items
                    );
            let desc_text = tokio::task::spawn_blocking({
                let llm0 = llm.clone();
                let p = prompt_desc.clone();
                move || {
                    llm0.chat(
                        &[ChatMessage {
                            role: "user".into(),
                            content: p,
                        }],
                        &react_core::llm::LlmCallOptions {
                            prompt_id: "react.catalog.enrich.field_descriptions_batch",
                            thread_id: None,
                            expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                            max_output_tokens: None,
                            temperature: None,
                            top_p: None,
                            reasoning_effort: None,
                        },
                    )
                }
            })
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();
            debug!(
                "{} LLM Enrich: description batch raw dataset_id='{}' fields=[{}]: {}",
                chrono::Utc::now().to_rfc3339(),
                dataset_id,
                fields.join(","),
                desc_text
            );
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&desc_text)
                .or_else(|_| super_extract_json_value(&desc_text))
            {
                if let Some(map) = v.get("descriptionByField").and_then(|m| m.as_object()) {
                    for (k, val) in map {
                        if !allowed_fields.contains(k) {
                            continue;
                        }
                        if let Some(s) = val.as_str() {
                            let s2 = s.trim();
                            let is_placeholder = s2.contains('<')
                                || s2.contains('>')
                                || s2.to_lowercase().contains("strict json");
                            if !s2.is_empty() && !is_placeholder {
                                desc_map_all.insert(k.clone(), s2.to_string());
                            }
                        }
                    }
                }
            }
            // Per-field fallback for any missing keys
            let mut missing: Vec<String> = fields
                .iter()
                .filter(|n| !desc_map_all.contains_key(*n))
                .cloned()
                .collect();
            if !missing.is_empty() {
                for fname in missing.drain(..) {
                    let item = format!(
                        "{{name: {}, role: {}, stats: {}}}",
                        fname,
                        role_by_field
                            .get(&fname)
                            .cloned()
                            .unwrap_or_else(|| "\"Unknown\"".to_string()),
                        serde_json::to_string(
                            stats_snip_by_field.get(&fname).unwrap_or(&"".to_string())
                        )
                        .unwrap_or_else(|_| "\"\"".to_string())
                    );
                    let single_prompt = format!(
                                "Return STRICT JSON only: {{\"descriptionByField\": {{ \"{fname}\": \"<≤20 words>\" }} }}.\nRules: one sentence (≤20 words) per field; JSON only; start with '{{' and end with '}}'.\nDataset: {ns}\nField details: [{item}]\n\nOutput JSON:",
                                fname = fname,
                                ns = dataset_id,
                                item = item
                            );
                    let single_text = tokio::task::spawn_blocking({
                        let llm0 = llm.clone();
                        let p = single_prompt.clone();
                        move || {
                            llm0.chat(
                                &[ChatMessage {
                                    role: "user".into(),
                                    content: p,
                                }],
                                &react_core::llm::LlmCallOptions {
                                    prompt_id: "react.catalog.enrich.field_description_single",
                                    thread_id: None,
                                    expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                                    max_output_tokens: None,
                                    temperature: None,
                                    top_p: None,
                                    reasoning_effort: None,
                                },
                            )
                        }
                    })
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .unwrap_or_default();
                    if let Ok(vs) = serde_json::from_str::<serde_json::Value>(&single_text)
                        .or_else(|_| super_extract_json_value(&single_text))
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
            let items2 = fields
                .iter()
                .map(|n| format!("{{name: {}}}", n))
                .collect::<Vec<String>>()
                .join(", ");
            let prompt_syn = format!(
                        "Return STRICT JSON only: {{\"synonymsByField\": {{ \"<field_name>\": [\"a\",\"b\"] }} }}.\nRules: 3–6 single-word synonyms, lowercase; JSON only; start with '{{' and end with '}}'. Keys MUST match FieldNames.\n\nDataset: {ns}\nFieldNames: {fnames}\nFields: [{items}]\n\nOutput JSON:",
                        ns = dataset_id,
                        fnames = field_names_json,
                        items = items2
                    );
            let syn_text = tokio::task::spawn_blocking({
                let llm0 = llm.clone();
                let p = prompt_syn.clone();
                move || {
                    llm0.chat(
                        &[ChatMessage {
                            role: "user".into(),
                            content: p,
                        }],
                        &react_core::llm::LlmCallOptions {
                            prompt_id: "react.catalog.enrich.field_synonyms_batch",
                            thread_id: None,
                            expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                            max_output_tokens: None,
                            temperature: None,
                            top_p: None,
                            reasoning_effort: None,
                        },
                    )
                }
            })
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();
            debug!(
                "{} LLM Enrich: synonyms batch raw dataset_id='{}' fields=[{}]: {}",
                chrono::Utc::now().to_rfc3339(),
                dataset_id,
                fields.join(","),
                syn_text
            );
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&syn_text)
                .or_else(|_| super_extract_json_value(&syn_text))
            {
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
                                    let is_placeholder = t.contains('<')
                                        || t.contains('>')
                                        || t.contains("strict json");
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
            let items3 = fields
                .iter()
                .map(|n| format!("{{name: {}}}", n))
                .collect::<Vec<String>>()
                .join(", ");
            let prompt_pu = format!(
                        "Return STRICT JSON only: {{\"piiUnitsByField\": {{ \"<field_name>\": {{\"pii\": \"none|low|medium|high\", \"units\": \"<units or format>\"}} }} }}.\nRules: units may be null if not applicable; JSON only; start with '{{' and end with '}}'. Keys MUST match FieldNames.\n\nDataset: {ns}\nFieldNames: {fnames}\nFields: [{items}]\n\nOutput JSON:",
                        ns = dataset_id,
                        fnames = field_names_json,
                        items = items3
                    );
            let pu_text = tokio::task::spawn_blocking({
                let llm0 = llm.clone();
                let p = prompt_pu.clone();
                move || {
                    llm0.chat(
                        &[ChatMessage {
                            role: "user".into(),
                            content: p,
                        }],
                        &react_core::llm::LlmCallOptions {
                            prompt_id: "react.catalog.enrich.field_pii_units_batch",
                            thread_id: None,
                            expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                            max_output_tokens: None,
                            temperature: None,
                            top_p: None,
                            reasoning_effort: None,
                        },
                    )
                }
            })
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();
            debug!(
                "{} LLM Enrich: pii/units batch raw dataset_id='{}' fields=[{}]: {}",
                chrono::Utc::now().to_rfc3339(),
                dataset_id,
                fields.join(","),
                pu_text
            );
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pu_text)
                .or_else(|_| super_extract_json_value(&pu_text))
            {
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
                                let is_placeholder = t.contains('<')
                                    || t.contains('>')
                                    || t.to_lowercase().contains("strict json");
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
        let value =
            serde_yaml::from_str::<serde_yaml::Value>(&yaml).unwrap_or(serde_yaml::Value::Null);
        let json_equiv = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
        let _ = storage.put_json(&key, &json_equiv).await;
    }
}

/// Enrich all datasets with LLM at the end of discovery.
pub async fn run_llm_enrichment_all(
    storage: Arc<dyn crate::adapters::storage::StorageAdapter>,
    keyspace: Arc<dyn crate::providers::Keyspace>,
    llm: Arc<dyn crate::llm::LargeLanguageModel>,
    scope: &crate::providers::RequestScope,
    dataset_ids: &HashMap<String, crate::discover::Metadata>,
    llm_timeout_secs: u64,
    llm_batch_size: usize,
) {
    // Engine-agnostic: iterate provided dataset ids (no sqlrt/registry coupling).
    for ds in dataset_ids.keys() {
        enrich_dataset_with_llm(
            storage.clone(),
            keyspace.clone(),
            llm.clone(),
            scope,
            ds,
            llm_timeout_secs,
            llm_batch_size,
        )
        .await;
    }
}

/// Enrich GLOBAL project-level context (business meaning + audiences) from all dataset catalogs.
///
/// This is intentionally global-only (no per-field additions). It runs in batches over *all* datasets
/// so we don't assume which ones are important up front.
pub async fn run_llm_global_context_enrichment_all(
    storage: Arc<dyn crate::adapters::storage::StorageAdapter>,
    keyspace: Arc<dyn crate::providers::Keyspace>,
    llm: Arc<dyn crate::llm::LargeLanguageModel>,
    scope: &crate::providers::RequestScope,
    dataset_ids: &HashMap<String, crate::discover::Metadata>,
    llm_timeout_secs: u64,
) {
    // Deterministic order.
    let mut dss: Vec<String> = dataset_ids.keys().cloned().collect();
    dss.sort();

    // Load existing global context if any (model should update, not rewrite).
    let mut global = read_global_semantic_context(&storage, &keyspace, scope)
        .await
        .unwrap_or_default();

    // Chunk datasets by approximate character budget to keep prompts bounded.
    let mut batch: Vec<serde_json::Value> = Vec::new();
    let mut batch_chars: usize = 0;
    let max_batch_chars: usize = 85_000; // conservative prompt budget; LLM backend dependent

    let mut flush_batch = |batch: &mut Vec<serde_json::Value>,
                           global: &mut react_core::providers::catalog::types::GlobalSemanticContext|
     -> Option<String> {
        if batch.is_empty() {
            return None;
        }
        let existing = serde_json::to_string_pretty(global).unwrap_or_else(|_| "{}".to_string());
        let input = serde_json::to_string_pretty(&batch).unwrap_or_else(|_| "[]".to_string());
        batch.clear();
        Some(format!(
            "You are inferring project-level business context from datasets.\n\
Return STRICT JSON only for this schema:\n\
{{\n\
  \"version\": 1,\n\
  \"built_at_epoch_secs\": <optional int>,\n\
  \"audiences\": [{{\"audience\": string, \"confidence\": number, \"evidence\": [string...]}}...],\n\
  \"context_bullets\": [{{\"text\": string, \"confidence\": number, \"evidence\": [string...]}}...],\n\
  \"dataset_groups\": [{{\"group_name\": string, \"dataset_ids\": [string...], \"confidence\": number, \"evidence\": [string...]}}...],\n\
  \"assumptions_and_gaps\": [{{\"text\": string, \"confidence\": number, \"evidence\": [string...], \"suggested_probe\": string|null}}...]\n\
}}\n\
\n\
Rules:\n\
- Only include entries when confidence is genuinely high (>= {min_conf}).\n\
- Evidence must cite dataset_id and concrete schema/stats clues (field names/types/stats).\n\
- Do NOT invent company/domain specifics; prefer general-but-useful analytics context.\n\
- Merge with existing context: keep good prior entries; add/adjust only when the new batch provides strong evidence.\n\
\n\
Existing global context JSON:\n\
{existing}\n\
\n\
Dataset batch context (catalog summaries):\n\
{input}\n\
\n\
Output JSON only:",
            min_conf = GLOBAL_CONTEXT_MIN_CONFIDENCE,
            existing = existing,
            input = input
        ))
    };

    for ds in dss.iter() {
        let key = keyspace.catalog_key(scope, ds);
        let Ok(cat_json) = storage.get_json(&key).await else {
            continue;
        };
        let compact = compact_catalog_for_global_context(ds, &cat_json);
        let add_chars = serde_json::to_string(&compact).map(|s| s.len()).unwrap_or(0);
        if !batch.is_empty() && (batch_chars + add_chars) > max_batch_chars {
            if let Some(prompt) = flush_batch(&mut batch, &mut global) {
                let text_opt = if llm_timeout_secs == 0 {
                    let llm0 = llm.clone();
                    tokio::task::spawn_blocking(move || {
                        llm0.chat(
                            &[ChatMessage {
                                role: "user".into(),
                                content: prompt,
                            }],
                            &react_core::llm::LlmCallOptions {
                                prompt_id: "react.catalog.enrich.global_semantic_context",
                                thread_id: None,
                                expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                                max_output_tokens: None,
                                temperature: None,
                                top_p: None,
                                reasoning_effort: None,
                            },
                        )
                    })
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                } else {
                    let llm0 = llm.clone();
                    tokio::time::timeout(
                        std::time::Duration::from_secs(llm_timeout_secs),
                        tokio::task::spawn_blocking(move || {
                            llm0.chat(
                                &[ChatMessage {
                                    role: "user".into(),
                                    content: prompt,
                                }],
                                &react_core::llm::LlmCallOptions {
                                    prompt_id: "react.catalog.enrich.global_semantic_context",
                                    thread_id: None,
                                    expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                                    max_output_tokens: None,
                                    temperature: None,
                                    top_p: None,
                                    reasoning_effort: None,
                                },
                            )
                        }),
                    )
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .and_then(|r| r.ok())
                };
                if let Some(text) = text_opt {
                    if let Ok(v) = super_extract_json_value(&text) {
                        if let Ok(parsed) = serde_json::from_value::<
                            react_core::providers::catalog::types::GlobalSemanticContext,
                        >(v)
                        {
                            global = clamp_and_filter_global_context(parsed);
                            global.version = global.version.max(1);
                            global.built_at_epoch_secs =
                                Some((chrono::Utc::now().timestamp()).max(0) as u64);
                            write_global_semantic_context(&storage, &keyspace, scope, &global).await;
                        }
                    }
                }
            }
            batch_chars = 0;
        }
        batch_chars += add_chars;
        batch.push(compact);
    }

    // Final flush.
    if let Some(prompt) = flush_batch(&mut batch, &mut global) {
        let text_opt = if llm_timeout_secs == 0 {
            let llm0 = llm.clone();
            tokio::task::spawn_blocking(move || {
                llm0.chat(
                    &[ChatMessage {
                        role: "user".into(),
                        content: prompt,
                    }],
                    &react_core::llm::LlmCallOptions {
                        prompt_id: "react.catalog.enrich.global_semantic_context",
                        thread_id: None,
                        expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                        max_output_tokens: None,
                        temperature: None,
                        top_p: None,
                        reasoning_effort: None,
                    },
                )
            })
            .await
            .ok()
            .and_then(|r| r.ok())
        } else {
            let llm0 = llm.clone();
            tokio::time::timeout(
                std::time::Duration::from_secs(llm_timeout_secs),
                tokio::task::spawn_blocking(move || {
                    llm0.chat(
                        &[ChatMessage {
                            role: "user".into(),
                            content: prompt,
                        }],
                        &react_core::llm::LlmCallOptions {
                            prompt_id: "react.catalog.enrich.global_semantic_context",
                            thread_id: None,
                            expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                            max_output_tokens: None,
                            temperature: None,
                            top_p: None,
                            reasoning_effort: None,
                        },
                    )
                }),
            )
            .await
            .ok()
            .and_then(|r| r.ok())
            .and_then(|r| r.ok())
        };
        if let Some(text) = text_opt {
            if let Ok(v) = super_extract_json_value(&text) {
                if let Ok(parsed) = serde_json::from_value::<
                    react_core::providers::catalog::types::GlobalSemanticContext,
                >(v)
                {
                    global = clamp_and_filter_global_context(parsed);
                    global.version = global.version.max(1);
                    global.built_at_epoch_secs =
                        Some((chrono::Utc::now().timestamp()).max(0) as u64);
                    write_global_semantic_context(&storage, &keyspace, scope, &global).await;
                }
            }
        }
    }
}

// NOTE: legacy wrapper removed. Call `run_llm_enrichment_all(storage, keyspace, llm, scope, dataset_ids)` instead.

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
