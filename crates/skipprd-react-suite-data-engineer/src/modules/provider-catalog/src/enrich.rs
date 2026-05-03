// TODO(item-76): This file (~500 lines) handles dataset enrichment, field enrichment,
// and global semantic context enrichment. Consider decomposing into:
//   - enrich_dataset.rs (dataset description LLM flow)
//   - enrich_fields.rs (field descriptions, synonyms, PII/units)
//   - enrich_global.rs (global semantic context aggregation)
use react_core::keyspace::encode_key_component;
use react_core::keyspace::Keyspace;
use react_core::llm::{ChatMessage, ChatRole, LargeLanguageModel};
use react_core::scope::RequestScope;
use react_core::storage::StorageAdapter;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;

use crate::types::{CatalogField, DataCatalog};

const GLOBAL_CONTEXT_MIN_CONFIDENCE: f32 = 0.80;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DatasetDescriptionCompile {
    description: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldDescriptionsCompile {
    #[serde(rename = "descriptionByField")]
    description_by_field: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldSynonymsCompile {
    #[serde(rename = "synonymsByField")]
    synonyms_by_field: HashMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldPiiUnitsCompile {
    #[serde(rename = "piiUnitsByField")]
    pii_units_by_field: HashMap<String, FieldPiiUnit>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldPiiUnit {
    pii: Option<String>,
    units: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalSemanticContextCompile {
    version: Option<u32>,
    built_at_epoch_secs: Option<u64>,
    audiences: Vec<GlobalAudienceCompile>,
    context_bullets: Vec<GlobalContextBulletCompile>,
    dataset_groups: Vec<GlobalDatasetGroupCompile>,
    assumptions_and_gaps: Vec<GlobalAssumptionGapCompile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalAudienceCompile {
    audience: String,
    confidence: f32,
    evidence: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalContextBulletCompile {
    text: String,
    confidence: f32,
    evidence: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalDatasetGroupCompile {
    group_name: String,
    dataset_ids: Vec<String>,
    confidence: f32,
    evidence: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalAssumptionGapCompile {
    text: String,
    confidence: f32,
    evidence: Vec<String>,
    suggested_probe: Option<String>,
}

// TODO(item-75): The timeout-wrapped spawn_blocking pattern below is repeated in
// both `llm_reason_pass` and `llm_compile_pass_json`. Extract a shared helper like
// `llm_call_with_timeout(llm, messages, opts, timeout_secs) -> Option<String>`.
async fn llm_reason_pass(
    llm: Arc<dyn LargeLanguageModel>,
    prompt: String,
    prompt_id: &'static str,
    llm_timeout_secs: u64,
) -> Option<String> {
    let opts = react_core::llm::LlmCallOptions {
        prompt_id,
        expected_format: react_core::llm::LlmExpectedFormat::Text,
        ..Default::default()
    };
    if llm_timeout_secs == 0 {
        tokio::task::spawn_blocking(move || {
            llm.chat(
                &[ChatMessage {
                    role: ChatRole::User,
                    content: prompt,
                }],
                &opts,
            )
        })
        .await
        .ok()
        .and_then(|r| r.ok())
    } else {
        tokio::time::timeout(
            std::time::Duration::from_secs(llm_timeout_secs),
            tokio::task::spawn_blocking(move || {
                llm.chat(
                    &[ChatMessage {
                        role: ChatRole::User,
                        content: prompt,
                    }],
                    &opts,
                )
            }),
        )
        .await
        .ok()
        .and_then(|r| r.ok())
        .and_then(|r| r.ok())
    }
}

async fn llm_compile_pass_json(
    llm: Arc<dyn LargeLanguageModel>,
    prompt: String,
    prompt_id: &'static str,
    llm_timeout_secs: u64,
) -> Option<serde_json::Value> {
    let opts = react_core::llm::LlmCallOptions {
        prompt_id,
        expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
        ..Default::default()
    };
    let text = if llm_timeout_secs == 0 {
        tokio::task::spawn_blocking(move || {
            llm.chat(
                &[ChatMessage {
                    role: ChatRole::User,
                    content: prompt,
                }],
                &opts,
            )
        })
        .await
        .ok()
        .and_then(|r| r.ok())
    } else {
        tokio::time::timeout(
            std::time::Duration::from_secs(llm_timeout_secs),
            tokio::task::spawn_blocking(move || {
                llm.chat(
                    &[ChatMessage {
                        role: ChatRole::User,
                        content: prompt,
                    }],
                    &opts,
                )
            }),
        )
        .await
        .ok()
        .and_then(|r| r.ok())
        .and_then(|r| r.ok())
    }?;
    serde_json::from_str::<serde_json::Value>(&text)
        .or_else(|_| super_extract_json_value(&text))
        .ok()
}

fn global_semantic_key(keyspace: &Arc<dyn Keyspace>, scope: &RequestScope) -> String {
    keyspace.scoped_key(
        scope,
        &[
            "semantic",
            &format!(
                "{}.yaml",
                encode_key_component(crate::types::GLOBAL_SEMANTIC_DATASET_ID)
            ),
        ],
    )
}

async fn read_global_semantic_context(
    storage: &Arc<dyn StorageAdapter>,
    keyspace: &Arc<dyn Keyspace>,
    scope: &RequestScope,
) -> Option<crate::types::GlobalSemanticContext> {
    let key = global_semantic_key(keyspace, scope);
    let v = storage.get_json(&key).await.ok()?;
    serde_json::from_value::<crate::types::GlobalSemanticContext>(v).ok()
}

async fn write_global_semantic_context(
    storage: &Arc<dyn StorageAdapter>,
    keyspace: &Arc<dyn Keyspace>,
    scope: &RequestScope,
    ctx: &crate::types::GlobalSemanticContext,
) -> Result<(), String> {
    let key = global_semantic_key(keyspace, scope);
    let json_equiv = crate::utils::yaml_to_json_value(ctx)?;
    storage
        .put_json(&key, &json_equiv)
        .await
        .map_err(|e| e.to_string())
}

fn clamp_and_filter_global_context(
    mut ctx: crate::types::GlobalSemanticContext,
) -> crate::types::GlobalSemanticContext {
    ctx.audiences
        .retain(|a| a.confidence >= GLOBAL_CONTEXT_MIN_CONFIDENCE && !a.audience.trim().is_empty());
    ctx.context_bullets
        .retain(|b| b.confidence >= GLOBAL_CONTEXT_MIN_CONFIDENCE && !b.text.trim().is_empty());
    ctx.dataset_groups.retain(|g| {
        g.confidence >= GLOBAL_CONTEXT_MIN_CONFIDENCE
            && !g.group_name.trim().is_empty()
            && !g.dataset_ids.is_empty()
    });
    ctx.assumptions_and_gaps
        .retain(|a| a.confidence >= GLOBAL_CONTEXT_MIN_CONFIDENCE && !a.text.trim().is_empty());

    ctx.audiences.truncate(12);
    ctx.context_bullets.truncate(24);
    ctx.dataset_groups.truncate(40);
    ctx.assumptions_and_gaps.truncate(24);

    let mut seen = std::collections::HashSet::<String>::new();
    ctx.audiences
        .retain(|a| seen.insert(a.audience.trim().to_string()));
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
    let max_fields = 40usize;
    let mut out_fields: Vec<serde_json::Value> = Vec::new();
    for f in fields.into_iter().take(max_fields) {
        let name = f
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if name.trim().is_empty() {
            continue;
        }
        let ty = f
            .get("type")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let role = f
            .get("role")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let stats = f.get("stats").cloned().unwrap_or(serde_json::Value::Null);
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

fn invalid_placeholder_text(s: &str) -> bool {
    let t = s.trim().to_ascii_lowercase();
    t.is_empty() || t.contains('<') || t.contains('>') || t.contains("strict json")
}

fn valid_ascii_span(s: &str, min_len: usize, max_len: usize) -> bool {
    let t = s.trim();
    t.len() >= min_len && t.len() <= max_len && t.chars().all(|c| c.is_ascii() && !c.is_control())
}

fn deterministic_global_context_from_compact(
    compact_batch: &[serde_json::Value],
) -> crate::types::GlobalSemanticContext {
    let dataset_ids: Vec<String> = compact_batch
        .iter()
        .filter_map(|v| {
            v.get("dataset_id")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    let mut bullets = vec![crate::types::GlobalContextBullet {
        text: "Project models warehouse datasets for analytics use-cases.".to_string(),
        confidence: 0.90,
        evidence: dataset_ids
            .iter()
            .take(5)
            .map(|d| format!("dataset_id={}", d))
            .collect(),
    }];
    if !dataset_ids.is_empty() {
        bullets.push(crate::types::GlobalContextBullet {
            text: "Catalog refresh confirms schema-driven planning context is available."
                .to_string(),
            confidence: 0.85,
            evidence: dataset_ids
                .iter()
                .take(5)
                .map(|d| format!("dataset_id={}", d))
                .collect(),
        });
    }
    crate::types::GlobalSemanticContext {
        version: 1,
        built_at_epoch_secs: Some((chrono::Utc::now().timestamp()).max(0) as u64),
        audiences: vec![crate::types::GlobalAudience {
            audience: "Analytics engineering and data consumers".to_string(),
            confidence: 0.90,
            evidence: dataset_ids
                .iter()
                .take(5)
                .map(|d| format!("dataset_id={}", d))
                .collect(),
        }],
        context_bullets: bullets,
        dataset_groups: if dataset_ids.is_empty() {
            vec![]
        } else {
            vec![crate::types::GlobalDatasetGroup {
                group_name: "Discovered project datasets".to_string(),
                dataset_ids: dataset_ids.clone(),
                confidence: 0.85,
                evidence: dataset_ids
                    .iter()
                    .take(5)
                    .map(|d| format!("dataset_id={}", d))
                    .collect(),
            }]
        },
        assumptions_and_gaps: vec![crate::types::GlobalAssumptionGap {
            text: "Business semantics inferred from available schema and stats; validate domain-specific definitions during planning.".to_string(),
            confidence: 0.85,
            evidence: dataset_ids
                .iter()
                .take(5)
                .map(|d| format!("dataset_id={}", d))
                .collect(),
            suggested_probe: Some(
                "Validate metric definitions, grain, and audience-specific reporting needs."
                    .to_string(),
            ),
        }],
    }
}

pub async fn enrich_dataset_with_llm(
    storage: Arc<dyn StorageAdapter>,
    keyspace: Arc<dyn Keyspace>,
    llm: Arc<dyn LargeLanguageModel>,
    scope: &RequestScope,
    dataset_id: &str,
    llm_timeout_secs: u64,
    llm_batch_size: usize,
) -> Result<bool, String> {
    let semantic = crate::infer::infer_semantic_model_async(
        storage.clone(),
        keyspace.clone(),
        scope,
        dataset_id,
    )
    .await;

    if !semantic.fields.is_empty() {
        let field_names: Vec<String> = semantic.fields.iter().map(|f| f.name.clone()).collect();
        let mut lines: Vec<String> = Vec::new();
        lines.push(format!("Dataset: {}", dataset_id));
        lines.push(format!(
            "Fields: {}",
            field_names
                .iter()
                .take(10)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
        let reason_prompt = format!(
            "You are preparing semantic notes for a dataset catalog entry.\n\
Return plain text only (no JSON).\n\
Focus only on dataset purpose and business meaning using the provided dataset id and fields.\n\
Do not include templates like <...> and do not restate instructions.\n\n{}",
            lines.join("\n")
        );
        let reason_memo = llm_reason_pass(
            llm.clone(),
            reason_prompt,
            "react.catalog.enrich.dataset.reason",
            llm_timeout_secs,
        )
        .await
        .unwrap_or_default();
        let compile_prompt = format!(
            "Compile the memo into strict JSON only.\n\
Return exactly: {{\"description\":\"...\"}} and no other keys.\n\
Rules:\n\
- One or two short sentences, <= 40 words.\n\
- ASCII printable text only.\n\
- No placeholders like <...>, no markdown, no commentary.\n\n\
Dataset: {dataset_id}\n\
Fields: {fields}\n\
Reasoning memo:\n{memo}\n\nOutput JSON only:",
            dataset_id = dataset_id,
            fields = field_names.join(", "),
            memo = reason_memo
        );
        let mut summary_raw: Option<String> = None;
        if let Some(v) = llm_compile_pass_json(
            llm.clone(),
            compile_prompt,
            "react.catalog.enrich.dataset.compile",
            llm_timeout_secs,
        )
        .await
        {
            if let Ok(parsed) = serde_json::from_value::<DatasetDescriptionCompile>(v) {
                summary_raw = Some(parsed.description);
            }
        }
        let summary_invalid = summary_raw
            .as_deref()
            .map(|s| !valid_ascii_span(s, 8, 220) || invalid_placeholder_text(s))
            .unwrap_or(true);
        if summary_invalid {
            let followup_prompt = format!(
                "The previous compile was missing/invalid.\n\
Return exactly {{\"description\":\"...\"}} with one or two short ASCII sentences (<=40 words), no placeholders.\n\
Dataset: {dataset_id}\n\
Fields: {fields}\n\
Output JSON only:",
                dataset_id = dataset_id,
                fields = field_names.join(", ")
            );
            if let Some(v) = llm_compile_pass_json(
                llm.clone(),
                followup_prompt,
                "react.catalog.enrich.dataset.compile_followup",
                llm_timeout_secs,
            )
            .await
            {
                if let Ok(parsed) = serde_json::from_value::<DatasetDescriptionCompile>(v) {
                    summary_raw = Some(parsed.description);
                }
            }
        }
        if let Some(text) =
            summary_raw.filter(|s| valid_ascii_span(s, 8, 220) && !invalid_placeholder_text(s))
        {
            debug!(
                "{} LLM Enrich: dataset description compiled for dataset_id='{}': {}",
                chrono::Utc::now().to_rfc3339(),
                dataset_id,
                text
            );
            let key = keyspace.scoped_key(
                scope,
                &[
                    "catalog",
                    &format!("{}.yaml", encode_key_component(dataset_id)),
                ],
            );
            if let Ok(mut v) = storage.get_json(&key).await {
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
                            serde_json::Value::String(text.clone()),
                        )
                    });
                    storage
                        .put_json(&key, &v)
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
        } else {
            return Err(format!(
                "catalog dataset description compile incomplete for '{}': missing valid description after follow-up",
                dataset_id
            ));
        }
    }

    let ns_stats: Option<react_suite_data_engineer::providers::DatasetFieldStats> = {
        let key = keyspace.scoped_key(
            scope,
            &[
                "catalog",
                &format!("{}.yaml", encode_key_component(dataset_id)),
            ],
        );
        match storage.get_json(&key).await {
            Ok(val) => {
                crate::stats_from_catalog::dataset_field_stats_from_catalog_json(dataset_id, &val)
            }
            Err(_) => None,
        }
    };
    let key = keyspace.scoped_key(
        scope,
        &[
            "catalog",
            &format!("{}.yaml", encode_key_component(dataset_id)),
        ],
    );
    if let Ok(val) = storage.get_json(&key).await {
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
                            let stats_opt = f.get("stats").cloned();
                            let stats_lite = stats_opt.and_then(|st| {
                                serde_json::from_value::<crate::types::FieldStatsLite>(st).ok()
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
                                    serde_json::from_value::<crate::types::StructureKind>(x.clone())
                                        .ok()
                                }),
                                access_descriptor: f.get("access_descriptor").and_then(|x| {
                                    serde_json::from_value::<crate::types::AccessDescriptor>(
                                        x.clone(),
                                    )
                                    .ok()
                                }),
                                description: desc,
                                synonyms: syns,
                                pii_sensitivity: pii,
                                units_or_format: units,
                                role,
                                stats_status: if stats_lite.is_some() {
                                    crate::types::StatsStatus::Collected
                                } else {
                                    crate::types::StatsStatus::SchemaOnly
                                },
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
                            stats_status: crate::types::StatsStatus::SchemaOnly,
                            stats: None,
                        })
                        .collect()
                }),
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
                .and_then(|x| serde_json::from_value::<crate::types::DatasetStats>(x.clone()).ok()),
            built_at_epoch_secs: val.get("built_at_epoch_secs").and_then(|x| x.as_u64()),
        };
        let field_names: Vec<String> = catalog.fields.iter().map(|f| f.name.clone()).collect();
        let batch_size = llm_batch_size;

        let chunks: Vec<Vec<String>> = field_names
            .chunks(batch_size.max(1))
            .map(|c| c.to_vec())
            .collect();

        let mut desc_map_all = std::collections::HashMap::<String, String>::new();
        let mut syn_map_all = std::collections::HashMap::<String, Vec<String>>::new();
        let mut pii_map_all = std::collections::HashMap::<String, String>::new();
        let mut units_map_all = std::collections::HashMap::<String, String>::new();

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
            let reason_prompt_desc = format!(
                "Write semantic field-description notes in plain text only (no JSON).\n\
Focus only on concise per-field meaning for this dataset.\n\
Dataset: {ns}\nFieldNames: {fnames}\nField details: [{items}]",
                ns = dataset_id,
                fnames = field_names_json,
                items = items
            );
            let desc_reason = llm_reason_pass(
                llm.clone(),
                reason_prompt_desc,
                "react.catalog.enrich.fields.reason",
                llm_timeout_secs,
            )
            .await
            .unwrap_or_default();
            let compile_desc = format!(
                "Compile the notes into strict JSON only.\n\
Return exactly {{\"descriptionByField\": {{\"<field>\": \"...\"}}}}.\n\
Rules: keys MUST match FieldNames exactly; one sentence <= 20 words per field; ASCII only; no placeholders <...>; no extra keys.\n\
Dataset: {ns}\nFieldNames: {fnames}\nReasoning notes:\n{memo}\n\nOutput JSON only:",
                ns = dataset_id,
                fnames = field_names_json,
                memo = desc_reason
            );
            if let Some(v) = llm_compile_pass_json(
                llm.clone(),
                compile_desc,
                "react.catalog.enrich.fields.compile_descriptions",
                llm_timeout_secs,
            )
            .await
            {
                if let Ok(parsed) = serde_json::from_value::<FieldDescriptionsCompile>(v) {
                    for (k, val) in parsed.description_by_field {
                        if !allowed_fields.contains(&k) {
                            continue;
                        }
                        let s2 = val.trim();
                        let is_placeholder = s2.contains('<')
                            || s2.contains('>')
                            || s2.to_lowercase().contains("strict json");
                        if !s2.is_empty() && !is_placeholder {
                            desc_map_all.insert(k, s2.to_string());
                        }
                    }
                }
            }
            let missing_desc: Vec<String> = fields
                .iter()
                .filter(|f| !desc_map_all.contains_key((*f).as_str()))
                .cloned()
                .collect();
            if !missing_desc.is_empty() {
                let missing_json = format!(
                    "[{}]",
                    missing_desc
                        .iter()
                        .map(|n| format!("\"{}\"", n))
                        .collect::<Vec<String>>()
                        .join(",")
                );
                let followup_desc = format!(
                    "Complete missing field descriptions only.\n\
Return exactly {{\"descriptionByField\": {{\"<field>\": \"...\"}}}} for the requested missing fields.\n\
Rules: keys MUST match MissingFieldNames exactly; one sentence <= 20 words; ASCII only; no placeholders.\n\
Dataset: {ns}\nMissingFieldNames: {missing}\nReasoning notes:\n{memo}\n\nOutput JSON only:",
                    ns = dataset_id,
                    missing = missing_json,
                    memo = desc_reason
                );
                if let Some(v) = llm_compile_pass_json(
                    llm.clone(),
                    followup_desc,
                    "react.catalog.enrich.fields.compile_descriptions_followup",
                    llm_timeout_secs,
                )
                .await
                {
                    if let Ok(parsed) = serde_json::from_value::<FieldDescriptionsCompile>(v) {
                        for (k, val) in parsed.description_by_field {
                            if !missing_desc.iter().any(|m| m == &k) {
                                continue;
                            }
                            let s2 = val.trim();
                            if !s2.is_empty() && !invalid_placeholder_text(s2) {
                                desc_map_all.insert(k, s2.to_string());
                            }
                        }
                    }
                }
            }
            let still_missing_desc: Vec<String> = fields
                .iter()
                .filter(|f| !desc_map_all.contains_key((*f).as_str()))
                .cloned()
                .collect();
            if !still_missing_desc.is_empty() {
                return Err(format!(
                    "catalog field description compile incomplete for '{}': missing {} fields after follow-up ({})",
                    dataset_id,
                    still_missing_desc.len(),
                    still_missing_desc.into_iter().take(12).collect::<Vec<_>>().join(", ")
                ));
            }

            let compile_syn = format!(
                "Compile field synonym output from notes into strict JSON only.\n\
Return exactly {{\"synonymsByField\": {{\"<field>\": [\"a\",\"b\"]}}}}.\n\
Rules: keys MUST match FieldNames exactly; 3-6 lowercase single-word synonyms per field; no placeholders; no extra keys.\n\
Dataset: {ns}\nFieldNames: {fnames}\nReasoning notes:\n{memo}\n\nOutput JSON only:",
                ns = dataset_id,
                fnames = field_names_json,
                memo = desc_reason
            );
            if let Some(v) = llm_compile_pass_json(
                llm.clone(),
                compile_syn,
                "react.catalog.enrich.fields.compile_synonyms",
                llm_timeout_secs,
            )
            .await
            {
                if let Ok(parsed) = serde_json::from_value::<FieldSynonymsCompile>(v) {
                    for (k, arr) in parsed.synonyms_by_field {
                        if !allowed_fields.contains(&k) {
                            continue;
                        }
                        let mut out: Vec<String> = arr
                            .into_iter()
                            .map(|s| s.trim().to_lowercase())
                            .filter(|t| {
                                !t.is_empty()
                                    && !t.contains('<')
                                    && !t.contains('>')
                                    && !t.contains("strict json")
                            })
                            .collect();
                        out.dedup();
                        if !out.is_empty() {
                            syn_map_all.insert(k, out);
                        }
                    }
                }
            }

            let compile_pu = format!(
                "Compile field pii/units output from notes into strict JSON only.\n\
Return exactly {{\"piiUnitsByField\": {{\"<field>\": {{\"pii\": \"none|low|medium|high\", \"units\": \"...|null\"}}}}}}.\n\
Rules: keys MUST match FieldNames exactly; no placeholders; no extra keys.\n\
Dataset: {ns}\nFieldNames: {fnames}\nReasoning notes:\n{memo}\n\nOutput JSON only:",
                ns = dataset_id,
                fnames = field_names_json,
                memo = desc_reason
            );
            if let Some(v) = llm_compile_pass_json(
                llm.clone(),
                compile_pu,
                "react.catalog.enrich.fields.compile_pii_units",
                llm_timeout_secs,
            )
            .await
            {
                if let Ok(parsed) = serde_json::from_value::<FieldPiiUnitsCompile>(v) {
                    for (k, obj) in parsed.pii_units_by_field {
                        if !allowed_fields.contains(&k) {
                            continue;
                        }
                        if let Some(p) = obj.pii.as_deref() {
                            let p_l = p.to_lowercase();
                            if matches!(p_l.as_str(), "none" | "low" | "medium" | "high") {
                                pii_map_all.insert(k.clone(), p_l);
                            }
                        }
                        if let Some(u) = obj.units.as_deref() {
                            let t = u.trim();
                            let is_placeholder = t.contains('<')
                                || t.contains('>')
                                || t.to_lowercase().contains("strict json");
                            if !t.is_empty() && !is_placeholder {
                                units_map_all.insert(k, t.to_string());
                            }
                        }
                    }
                }
            }
        }

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
            if fld.role.is_none() {
                if let Some(r) = role_by_field.get(&fld.name) {
                    fld.role = Some(r.clone());
                }
            }
        }

        let json_equiv = crate::utils::yaml_to_json_value(&catalog)?;
        storage
            .put_json(&key, &json_equiv)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(true)
}

pub async fn run_llm_enrichment_all(
    storage: Arc<dyn StorageAdapter>,
    keyspace: Arc<dyn Keyspace>,
    llm: Arc<dyn LargeLanguageModel>,
    scope: &RequestScope,
    dataset_ids: &std::collections::HashSet<String>,
    llm_timeout_secs: u64,
    llm_batch_size: usize,
) -> Result<react_suite_data_engineer::providers::CatalogEnrichmentReport, String> {
    let mut report = react_suite_data_engineer::providers::CatalogEnrichmentReport {
        dataset_total: dataset_ids.len(),
        ..Default::default()
    };
    for ds in dataset_ids.iter() {
        match enrich_dataset_with_llm(
            storage.clone(),
            keyspace.clone(),
            llm.clone(),
            scope,
            ds,
            llm_timeout_secs,
            llm_batch_size,
        )
        .await
        {
            Ok(_) => report.dataset_enriched_ok += 1,
            Err(e) => {
                report.dataset_enriched_failed += 1;
                debug!(
                    "{} catalog enrichment failed for dataset_id='{}': {}",
                    chrono::Utc::now().to_rfc3339(),
                    ds,
                    e
                );
            }
        }
    }
    Ok(report)
}

async fn process_global_context_batch(
    llm: Arc<dyn LargeLanguageModel>,
    llm_timeout_secs: u64,
    scope: &RequestScope,
    keyspace: &Arc<dyn Keyspace>,
    storage: &Arc<dyn StorageAdapter>,
    global: &mut crate::types::GlobalSemanticContext,
    compact_batch: &[serde_json::Value],
) -> Result<bool, String> {
    if compact_batch.is_empty() {
        return Ok(false);
    }
    let existing = serde_json::to_string_pretty(global).unwrap_or_else(|_| "{}".to_string());
    let input = serde_json::to_string_pretty(compact_batch).unwrap_or_else(|_| "[]".to_string());
    let reason_prompt = format!(
        "You are inferring project-level business context from datasets.\n\
Return plain text reasoning only (no JSON).\n\
Focus on likely audiences, context bullets, dataset groupings, and assumptions/gaps grounded in evidence.\n\
Avoid invented company/domain specifics.\n\n\
Existing global context JSON:\n{existing}\n\n\
Dataset batch context (catalog summaries):\n{input}\n\n\
Output plain text only:"
    );
    let reason_memo = llm_reason_pass(
        llm.clone(),
        reason_prompt,
        "react.catalog.enrich.global.reason",
        llm_timeout_secs,
    )
    .await
    .unwrap_or_default();
    let compile_prompt = format!(
        "Compile the reasoning memo into strict JSON only for this schema:\n\
{{\n\
  \"version\": 1,\n\
  \"built_at_epoch_secs\": <optional int>,\n\
  \"audiences\": [{{\"audience\": string, \"confidence\": number, \"evidence\": [string...]}}...],\n\
  \"context_bullets\": [{{\"text\": string, \"confidence\": number, \"evidence\": [string...]}}...],\n\
  \"dataset_groups\": [{{\"group_name\": string, \"dataset_ids\": [string...], \"confidence\": number, \"evidence\": [string...]}}...],\n\
  \"assumptions_and_gaps\": [{{\"text\": string, \"confidence\": number, \"evidence\": [string...], \"suggested_probe\": string|null}}...]\n\
}}\n\
Rules:\n\
- Output JSON only.\n\
- No extra top-level keys, wrappers, commentary, or markdown.\n\
- Evidence must cite dataset_id and concrete schema/stats clues.\n\
- Confidence threshold target is >= {min_conf} for entries.\n\n\
Existing global context JSON:\n{existing}\n\n\
Dataset batch context:\n{input}\n\n\
Reasoning memo:\n{memo}\n\nOutput JSON only:",
        min_conf = GLOBAL_CONTEXT_MIN_CONFIDENCE,
        existing = existing,
        input = input,
        memo = reason_memo
    );
    let mut next = None;
    if let Some(v) = llm_compile_pass_json(
        llm,
        compile_prompt,
        "react.catalog.enrich.global.compile",
        llm_timeout_secs,
    )
    .await
    {
        if let Ok(parsed) = serde_json::from_value::<GlobalSemanticContextCompile>(v) {
            let typed = crate::types::GlobalSemanticContext {
                version: parsed.version.unwrap_or(1),
                built_at_epoch_secs: parsed.built_at_epoch_secs,
                audiences: parsed
                    .audiences
                    .into_iter()
                    .map(|a| crate::types::GlobalAudience {
                        audience: a.audience,
                        confidence: a.confidence,
                        evidence: a.evidence,
                    })
                    .collect(),
                context_bullets: parsed
                    .context_bullets
                    .into_iter()
                    .map(|b| crate::types::GlobalContextBullet {
                        text: b.text,
                        confidence: b.confidence,
                        evidence: b.evidence,
                    })
                    .collect(),
                dataset_groups: parsed
                    .dataset_groups
                    .into_iter()
                    .map(|g| crate::types::GlobalDatasetGroup {
                        group_name: g.group_name,
                        dataset_ids: g.dataset_ids,
                        confidence: g.confidence,
                        evidence: g.evidence,
                    })
                    .collect(),
                assumptions_and_gaps: parsed
                    .assumptions_and_gaps
                    .into_iter()
                    .map(|a| crate::types::GlobalAssumptionGap {
                        text: a.text,
                        confidence: a.confidence,
                        evidence: a.evidence,
                        suggested_probe: a.suggested_probe,
                    })
                    .collect(),
            };
            next = Some(clamp_and_filter_global_context(typed));
        }
    }
    let mut next = next.unwrap_or_else(|| deterministic_global_context_from_compact(compact_batch));
    next.version = next.version.max(1);
    next.built_at_epoch_secs = Some((chrono::Utc::now().timestamp()).max(0) as u64);
    write_global_semantic_context(storage, keyspace, scope, &next).await?;
    *global = next;
    Ok(true)
}

pub async fn run_llm_global_context_enrichment_all(
    storage: Arc<dyn StorageAdapter>,
    keyspace: Arc<dyn Keyspace>,
    llm: Arc<dyn LargeLanguageModel>,
    scope: &RequestScope,
    dataset_ids: &std::collections::HashSet<String>,
    llm_timeout_secs: u64,
) -> Result<bool, String> {
    let mut dss: Vec<String> = dataset_ids.iter().cloned().collect();
    dss.sort();

    let mut global = read_global_semantic_context(&storage, &keyspace, scope)
        .await
        .unwrap_or_default();

    let mut batch: Vec<serde_json::Value> = Vec::new();
    let mut batch_chars: usize = 0;
    let max_batch_chars: usize = 85_000;

    let flush_batch = |batch: &mut Vec<serde_json::Value>| -> Option<Vec<serde_json::Value>> {
        if batch.is_empty() {
            return None;
        }
        let out = batch.clone();
        batch.clear();
        Some(out)
    };
    let mut wrote = false;

    for ds in dss.iter() {
        let key = keyspace.scoped_key(
            scope,
            &["catalog", &format!("{}.yaml", encode_key_component(ds))],
        );
        let Ok(cat_json) = storage.get_json(&key).await else {
            continue;
        };
        let compact = compact_catalog_for_global_context(ds, &cat_json);
        let add_chars = serde_json::to_string(&compact)
            .map(|s| s.len())
            .unwrap_or(0);
        if !batch.is_empty() && (batch_chars + add_chars) > max_batch_chars {
            if let Some(compact_batch) = flush_batch(&mut batch) {
                wrote |= process_global_context_batch(
                    llm.clone(),
                    llm_timeout_secs,
                    scope,
                    &keyspace,
                    &storage,
                    &mut global,
                    &compact_batch,
                )
                .await?;
            }
            batch_chars = 0;
        }
        batch_chars += add_chars;
        batch.push(compact);
    }

    if let Some(compact_batch) = flush_batch(&mut batch) {
        wrote |= process_global_context_batch(
            llm.clone(),
            llm_timeout_secs,
            scope,
            &keyspace,
            &storage,
            &mut global,
            &compact_batch,
        )
        .await?;
    }
    if !wrote {
        let fallback = deterministic_global_context_from_compact(&[]);
        write_global_semantic_context(&storage, &keyspace, scope, &fallback).await?;
        return Ok(true);
    }
    Ok(true)
}

pub fn super_extract_json_value(text: &str) -> Result<serde_json::Value, serde_json::Error> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
        return Ok(v);
    }
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
    serde_json::from_str::<serde_json::Value>(text)
}
