use serde::{Deserialize, Serialize};
use serde_yaml::Value as YamlValue;
use std::collections::{HashMap, HashSet};

use react_core::agent::AgentCtx;
use react_core::storage::{retry_get_bytes, retry_list_prefix, retry_put_bytes};

use crate::project_fs;

#[derive(Clone, Debug, Default)]
pub struct ModelAllowedColumns {
    /// If empty, we cannot safely author tests; docs-only changes are allowed.
    pub allowed_columns: HashSet<String>,
    pub error: Option<String>,
}

/// Structured precheck failure. Drives the repair-subroutine prompt so the LLM can scope its
/// gather/reason pass to the relevant files without having to parse a free-form error string.
///
/// Each variant of [`PrecheckFailureKind`] describes a class of structural problem in the dbt
/// project tree that the agent can resolve by re-authoring or relocating LLM-owned files.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrecheckFailure {
    pub kind: PrecheckFailureKind,
    /// Human-readable summary of the structural issue (used as the `ValidationFailureContext`
    /// brief).
    pub brief: String,
    /// Relative paths the agent should examine first when repairing. Empty when the failure does
    /// not point at specific files (rare).
    pub suggested_targets: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PrecheckFailureKind {
    /// A staging model (`stg_*`) was defined in `models/schema.yml`. Staging docs/tests belong
    /// in `models/staging/*.yml` (one yml per source).
    SchemaModelMisplacement,
    /// A model name was declared in both `models/schema.yml` and `models/staging/*.yml`.
    DuplicateModelDefinition,
    /// Two or more `.sql` files share the same stem (dbt rejects duplicate model names).
    DuplicateSqlModelStem,
    /// A staging YAML declares columns that don't appear in the matching staging SQL.
    StagingSchemaColumnMismatch,
    /// A test definition under `tests:` violated dbt's "single-key mapping" rule.
    InvalidTestDefinitionShape,
    /// Parser failure or other structural defect not classified above.
    Other,
}

impl PrecheckFailure {
    pub fn other(brief: impl Into<String>) -> Self {
        Self {
            kind: PrecheckFailureKind::Other,
            brief: brief.into(),
            suggested_targets: Vec::new(),
        }
    }
}

fn yaml_as_mapping_mut(v: &mut YamlValue) -> Option<&mut serde_yaml::Mapping> {
    match v {
        YamlValue::Mapping(m) => Some(m),
        _ => None,
    }
}

fn yaml_get_str<'a>(v: &'a YamlValue, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

fn yaml_is_staging_model_name(name: &str) -> bool {
    crate::dataset_truth::is_staging_model_name(name)
}

fn extract_model_names_from_yml_text(yml_text: &str) -> Result<HashSet<String>, String> {
    let root: YamlValue =
        serde_yaml::from_str(yml_text).map_err(|e| format!("invalid YAML: {e}"))?;
    let mut out: HashSet<String> = HashSet::new();
    let YamlValue::Mapping(map) = root else {
        return Ok(out);
    };
    let Some(YamlValue::Sequence(models)) = map.get("models") else {
        return Ok(out);
    };
    for m in models.iter() {
        if let Some(n) = m
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            out.insert(n);
        }
    }
    Ok(out)
}

fn extract_declared_columns_from_model_entry(model: &YamlValue) -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(cols) = model.get("columns").and_then(|v| v.as_sequence()) else {
        return out;
    };
    for c in cols.iter() {
        let Some(n) = c
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        out.insert(n);
    }
    out
}

async fn collect_sql_model_name_collisions(
    ctx: &AgentCtx,
    limit: usize,
) -> Result<Vec<(String, Vec<String>)>, String> {
    let limit = limit.max(1).min(2000);
    let base = ctx
        .keyspace()
        .scoped_prefix(ctx.scope(), &["dbt"])
        .trim_end_matches('/')
        .to_string();
    let pref = format!("{}/models/", base);
    let mut keys = retry_list_prefix(ctx.storage().as_ref(), &pref)
        .await
        .unwrap_or_default();
    keys.sort();
    let mut by_stem: HashMap<String, Vec<String>> = HashMap::new();
    for key in keys.into_iter().filter(|k| k.ends_with(".sql")).take(limit) {
        if key.contains("/_versions/") {
            continue;
        }
        let rel = key
            .strip_prefix(&(base.clone() + "/"))
            .unwrap_or(key.as_str())
            .to_string();
        let stem = std::path::Path::new(&rel)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
            .trim()
            .to_string();
        if stem.is_empty() {
            continue;
        }
        by_stem.entry(stem).or_default().push(rel);
    }
    let mut out: Vec<(String, Vec<String>)> = by_stem
        .into_iter()
        .filter_map(|(stem, mut rels)| {
            rels.sort();
            rels.dedup();
            if rels.len() > 1 {
                Some((stem, rels))
            } else {
                None
            }
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

async fn collect_staging_model_names_from_ymls(
    ctx: &AgentCtx,
    limit: usize,
) -> Result<HashSet<String>, String> {
    let limit = limit.max(1).min(500);
    let base = ctx
        .keyspace()
        .scoped_prefix(ctx.scope(), &["dbt"])
        .trim_end_matches('/')
        .to_string();
    let pref = format!("{}/models/staging/", base);
    let mut keys = retry_list_prefix(ctx.storage().as_ref(), &pref)
        .await
        .unwrap_or_default();
    keys.sort();
    let mut out: HashSet<String> = HashSet::new();
    for k in keys.into_iter().filter(|k| k.ends_with(".yml")).take(limit) {
        let bytes = match retry_get_bytes(ctx.storage().as_ref(), &k).await {
            Ok(b) => b,
            Err(_) => continue,
        };
        let text = String::from_utf8_lossy(&bytes).to_string();
        let names = extract_model_names_from_yml_text(&text)?;
        out.extend(names);
    }
    Ok(out)
}

fn conservative_identifiers_in_where(where_sql: &str) -> HashSet<String> {
    // Very conservative tokenization: split on non [A-Za-z0-9_], keep identifier-like tokens.
    // This is intended only to catch obvious invented columns (raw_*, *_id_raw, etc).
    let mut out: HashSet<String> = HashSet::new();
    let mut cur = String::new();
    for ch in where_sql.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else {
            if !cur.is_empty() {
                out.insert(cur.clone());
                cur.clear();
            }
        }
    }
    if !cur.is_empty() {
        out.insert(cur);
    }
    // Drop obvious SQL keywords and boolean literals.
    let keywords: [&str; 20] = [
        "and", "or", "not", "null", "is", "in", "like", "true", "false", "case", "when", "then",
        "else", "end", "cast", "try_cast", "coalesce", "nullif", "trim", "lower",
    ];
    for k in keywords.iter() {
        out.remove(*k);
        out.remove(&k.to_ascii_uppercase());
        out.remove(&k.to_ascii_lowercase());
    }
    out
}

fn test_mapping_has_single_key(v: &YamlValue) -> bool {
    match v {
        YamlValue::Mapping(m) => m.len() == 1,
        _ => true, // strings etc are allowed
    }
}

fn mapping_single_key_name(v: &YamlValue) -> Option<String> {
    let YamlValue::Mapping(m) = v else {
        return None;
    };
    if m.len() != 1 {
        return None;
    }
    m.keys()
        .next()
        .and_then(|k| k.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn mapping_single_key_value<'a>(v: &'a YamlValue) -> Option<&'a YamlValue> {
    let YamlValue::Mapping(m) = v else {
        return None;
    };
    if m.len() != 1 {
        return None;
    }
    m.values().next()
}

fn sanitize_tests_seq(
    tests: &mut Vec<YamlValue>,
    allowed_cols: &HashSet<String>,
    forbid_tests: bool,
) -> Vec<String> {
    let mut warnings: Vec<String> = Vec::new();
    if forbid_tests {
        if !tests.is_empty() {
            warnings.push("removed tests because allowed_columns is empty/unproven".to_string());
        }
        tests.clear();
        return warnings;
    }
    // Keep only tests with valid shape; drop tests with where referencing unknown identifiers.
    tests.retain(|it| {
        if !test_mapping_has_single_key(it) {
            warnings.push("dropped malformed test dict (must have exactly one key)".to_string());
            return false;
        }
        // If config has `where:`, ensure identifiers are within allowed_cols.
        let Some(_test_name) = mapping_single_key_name(it) else {
            return true;
        };
        let Some(cfg) = mapping_single_key_value(it) else {
            return true;
        };
        let YamlValue::Mapping(cfgm) = cfg else {
            return true;
        };
        let Some(where_v) = cfgm.get("where") else {
            return true;
        };
        let Some(where_s) = where_v.as_str() else {
            return true;
        };
        let ids = conservative_identifiers_in_where(where_s);
        let unknown: Vec<String> = ids
            .into_iter()
            .filter(|t| !allowed_cols.contains(t))
            .collect();
        if !unknown.is_empty() {
            warnings.push(format!(
                "dropped test with ungrounded where identifiers: {}",
                unknown.join(", ")
            ));
            return false;
        }
        true
    });
    warnings
}

/// Sanitize `models/schema.yml` for a set of touched gold models.
///
/// Safe rules:
/// - Remove any `stg_*` model entries (schema ownership).
/// - For touched models, remove tests that reference ungrounded columns/where predicates.
pub fn sanitize_models_schema_yml(
    yml_text: &str,
    touched: &HashSet<String>,
    allowed_by_model: &HashMap<String, ModelAllowedColumns>,
) -> Result<(String, Vec<String>), String> {
    let mut root: YamlValue =
        serde_yaml::from_str(yml_text).map_err(|e| format!("schema.yml parse error: {e}"))?;
    let Some(map) = yaml_as_mapping_mut(&mut root) else {
        return Err("schema.yml root must be a mapping".to_string());
    };
    let models_key = YamlValue::String("models".to_string());
    let models_v = map
        .entry(models_key)
        .or_insert_with(|| YamlValue::Sequence(vec![]));
    let YamlValue::Sequence(models_seq) = models_v else {
        return Err("schema.yml models must be a sequence".to_string());
    };

    let mut warnings: Vec<String> = Vec::new();
    let mut out_models: Vec<YamlValue> = Vec::new();
    for m in models_seq.drain(..) {
        let YamlValue::Mapping(mut mm) = m else {
            out_models.push(m);
            continue;
        };
        let Some(name) = mm
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        else {
            out_models.push(YamlValue::Mapping(mm));
            continue;
        };

        if yaml_is_staging_model_name(&name) {
            warnings.push(format!(
                "removed staging model '{name}' from models/schema.yml (staging must live in models/staging/*.yml)"
            ));
            continue;
        }

        if !touched.contains(&name) {
            out_models.push(YamlValue::Mapping(mm));
            continue;
        }

        let allowed = allowed_by_model.get(&name).cloned().unwrap_or_default();
        let forbid_tests = allowed.allowed_columns.is_empty();
        if let Some(err) = allowed.error.as_ref() {
            warnings.push(format!(
                "{name}: allowed_columns unavailable ({err}); tests removed"
            ));
        }

        // Model-level tests.
        if let Some(YamlValue::Sequence(ref mut tests)) = mm.get_mut("tests") {
            warnings.extend(sanitize_tests_seq(
                tests,
                &allowed.allowed_columns,
                forbid_tests,
            ));
        }

        // Column-level tests: keep docs, but strip tests when ungrounded.
        if let Some(YamlValue::Sequence(ref mut cols)) = mm.get_mut("columns") {
            for c in cols.iter_mut() {
                let YamlValue::Mapping(ref mut cm) = c else {
                    continue;
                };
                let col_name = yaml_get_str(&YamlValue::Mapping(cm.clone()), "name")
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let col_is_allowed =
                    !col_name.is_empty() && allowed.allowed_columns.contains(&col_name);
                if let Some(YamlValue::Sequence(ref mut tests)) = cm.get_mut("tests") {
                    if forbid_tests || !col_is_allowed {
                        if !tests.is_empty() {
                            warnings.push(format!(
                                "{name}.{col_name}: removed tests (column not in allowed_columns or allowed_columns unavailable)"
                            ));
                        }
                        tests.clear();
                    } else {
                        warnings.extend(sanitize_tests_seq(tests, &allowed.allowed_columns, false));
                    }
                }
            }
        }

        out_models.push(YamlValue::Mapping(mm));
    }
    *models_seq = out_models;

    let out =
        serde_yaml::to_string(&root).map_err(|e| format!("failed to render schema.yml: {e}"))?;
    Ok((out, warnings))
}

fn yaml_norm_key(v: &YamlValue) -> String {
    serde_yaml::to_string(v).unwrap_or_else(|_| format!("{v:?}"))
}

fn dedupe_yaml_seq(seq: &mut Vec<YamlValue>) -> usize {
    let mut seen: HashSet<String> = HashSet::new();
    let before = seq.len();
    seq.retain(|v| seen.insert(yaml_norm_key(v)));
    before.saturating_sub(seq.len())
}

fn merge_model_entry(existing: &mut serde_yaml::Mapping, incoming: &serde_yaml::Mapping) {
    // Merge model-level tests.
    if let Some(YamlValue::Sequence(src_tests)) = incoming.get("tests") {
        let dst = existing
            .entry("tests".into())
            .or_insert_with(|| YamlValue::Sequence(vec![]));
        if let YamlValue::Sequence(dst_tests) = dst {
            dst_tests.extend(src_tests.iter().cloned());
            let _ = dedupe_yaml_seq(dst_tests);
        }
    }

    // Merge columns by name; keep first-seen metadata, append/dedupe tests.
    if let Some(YamlValue::Sequence(src_cols)) = incoming.get("columns") {
        let dst = existing
            .entry("columns".into())
            .or_insert_with(|| YamlValue::Sequence(vec![]));
        if let YamlValue::Sequence(dst_cols) = dst {
            let mut by_name: HashMap<String, usize> = HashMap::new();
            for (idx, c) in dst_cols.iter().enumerate() {
                let name = c
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                if let Some(n) = name {
                    by_name.entry(n).or_insert(idx);
                }
            }
            for c in src_cols.iter() {
                let Some(src_map) = c.as_mapping() else {
                    continue;
                };
                let name = src_map
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let Some(name) = name else {
                    continue;
                };
                if let Some(&dst_idx) = by_name.get(&name) {
                    let Some(dst_map) = dst_cols[dst_idx].as_mapping_mut() else {
                        continue;
                    };
                    for k in ["description", "data_type"] {
                        if !dst_map.contains_key(k) {
                            if let Some(v) = src_map.get(k) {
                                dst_map.insert(k.to_string().into(), v.clone());
                            }
                        }
                    }
                    if let Some(YamlValue::Sequence(src_tests)) = src_map.get("tests") {
                        let dst_tests_v = dst_map
                            .entry("tests".into())
                            .or_insert_with(|| YamlValue::Sequence(vec![]));
                        if let YamlValue::Sequence(dst_tests) = dst_tests_v {
                            dst_tests.extend(src_tests.iter().cloned());
                            let _ = dedupe_yaml_seq(dst_tests);
                        }
                    }
                } else {
                    dst_cols.push(c.clone());
                    by_name.insert(name, dst_cols.len().saturating_sub(1));
                }
            }
        }
    }
}

fn normalize_model_yaml_doc_for_dedupe(
    yml_text: &str,
    rel_path: &str,
) -> Result<(String, Vec<String>), String> {
    let mut root: YamlValue =
        serde_yaml::from_str(yml_text).map_err(|e| format!("{rel_path} parse error: {e}"))?;
    let Some(map) = yaml_as_mapping_mut(&mut root) else {
        return Ok((yml_text.to_string(), vec![]));
    };
    let Some(models_v) = map.get_mut("models") else {
        return Ok((yml_text.to_string(), vec![]));
    };
    let Some(models_seq) = models_v.as_sequence_mut() else {
        return Ok((yml_text.to_string(), vec![]));
    };

    let mut warnings: Vec<String> = Vec::new();
    let mut out: Vec<YamlValue> = Vec::new();
    let mut by_name: HashMap<String, usize> = HashMap::new();
    for m in models_seq.drain(..) {
        let Some(mm) = m.as_mapping() else {
            out.push(m);
            continue;
        };
        let name = mm
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let Some(name) = name else {
            out.push(m);
            continue;
        };
        if let Some(&idx) = by_name.get(&name) {
            if let Some(dst_map) = out[idx].as_mapping_mut() {
                merge_model_entry(dst_map, mm);
                warnings.push(format!(
                    "{rel_path}: merged duplicate model entry '{name}' under models[]"
                ));
            }
        } else {
            out.push(m);
            by_name.insert(name, out.len().saturating_sub(1));
        }
    }

    // Also dedupe test blocks inside each model/column.
    for m in out.iter_mut() {
        let Some(mm) = m.as_mapping_mut() else {
            continue;
        };
        if let Some(YamlValue::Sequence(tests)) = mm.get_mut("tests") {
            let removed = dedupe_yaml_seq(tests);
            if removed > 0 {
                warnings.push(format!(
                    "{rel_path}: removed {removed} duplicate model-level test(s)"
                ));
            }
        }
        if let Some(YamlValue::Sequence(cols)) = mm.get_mut("columns") {
            for c in cols.iter_mut() {
                let Some(cm) = c.as_mapping_mut() else {
                    continue;
                };
                if let Some(YamlValue::Sequence(tests)) = cm.get_mut("tests") {
                    let removed = dedupe_yaml_seq(tests);
                    if removed > 0 {
                        let col = cm
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("(unknown)");
                        warnings.push(format!(
                            "{rel_path}: removed {removed} duplicate test(s) for column '{col}'"
                        ));
                    }
                }
            }
        }
    }

    *models_seq = out;
    let rendered = serde_yaml::to_string(&root)
        .map_err(|e| format!("failed to render {rel_path}: {e}"))?
        .trim_start_matches("---\n")
        .to_string();
    Ok((rendered, warnings))
}

/// Normalize DBT schema YAML artifacts before validate/build to avoid deterministic compile loops
/// from duplicate model/test definitions.
pub async fn normalize_schema_artifacts_for_validate(
    ctx: &AgentCtx,
) -> Result<Vec<String>, String> {
    let mut notes: Vec<String> = Vec::new();
    let schema_rel = project_fs::MODELS_SCHEMA_YML;
    let schema_key = project_fs::join_storage_key(ctx, schema_rel);
    if let Ok(bytes) = retry_get_bytes(ctx.storage().as_ref(), &schema_key).await {
        let text = String::from_utf8_lossy(&bytes).to_string();
        let (normalized, mut warn) = normalize_model_yaml_doc_for_dedupe(&text, schema_rel)?;
        if normalized != text {
            retry_put_bytes(
                ctx.storage().as_ref(),
                &schema_key,
                normalized.as_bytes(),
                "text/yaml",
            )
            .await
            .map_err(|e| format!("failed to write {schema_rel}: {e}"))?;
            notes.push(format!(
                "normalized duplicate model/test entries in {schema_rel}"
            ));
        }
        notes.append(&mut warn);
    }

    let base = ctx
        .keyspace()
        .scoped_prefix(ctx.scope(), &["dbt"])
        .trim_end_matches('/')
        .to_string();
    let pref = format!("{}/models/staging/", base);
    let mut keys = retry_list_prefix(ctx.storage().as_ref(), &pref)
        .await
        .unwrap_or_default();
    keys.sort();
    for key in keys.into_iter().filter(|k| k.ends_with(".yml")) {
        let rel = key
            .strip_prefix(&(base.clone() + "/"))
            .unwrap_or(key.as_str())
            .to_string();
        let bytes = match retry_get_bytes(ctx.storage().as_ref(), &key).await {
            Ok(b) => b,
            Err(_) => continue,
        };
        let text = String::from_utf8_lossy(&bytes).to_string();
        let (normalized, mut warn) = normalize_model_yaml_doc_for_dedupe(&text, &rel)?;
        if normalized != text {
            retry_put_bytes(
                ctx.storage().as_ref(),
                &key,
                normalized.as_bytes(),
                "text/yaml",
            )
            .await
            .map_err(|e| format!("failed to write {rel}: {e}"))?;
            notes.push(format!("normalized duplicate model/test entries in {rel}"));
        }
        notes.append(&mut warn);
    }
    Ok(notes)
}

/// Cheap structural prechecks to avoid burning dbt_validate cycles on trivial YAML issues.
///
/// Failures route to the repair subroutine (not the author loopback) because they describe
/// structural relocations the LLM should solve in one focused gather→reason→apply cycle. See
/// [`crate::phase_validate`] for the routing site and [`PrecheckFailure`] for the structured
/// payload the repair prompt consumes.
///
/// Checks:
/// - `models/schema.yml` parses (if present)
/// - No `stg_*` models are defined in `models/schema.yml`
/// - All test dicts under any `tests:` list are single-key mappings
/// - Staging YAML columns exist in the corresponding staging SQL
pub async fn prevalidate_dbt_schema_artifacts(ctx: &AgentCtx) -> Result<(), PrecheckFailure> {
    let sql_name_collisions = collect_sql_model_name_collisions(ctx, 2000)
        .await
        .map_err(PrecheckFailure::other)?;
    if !sql_name_collisions.is_empty() {
        let lines: Vec<String> = sql_name_collisions
            .iter()
            .map(|(name, rels)| format!("{name}: {}", rels.join(" | ")))
            .collect();
        let suggested_targets: Vec<String> = sql_name_collisions
            .iter()
            .flat_map(|(_, rels)| rels.iter().cloned())
            .collect();
        return Err(PrecheckFailure {
            kind: PrecheckFailureKind::DuplicateSqlModelStem,
            brief: format!(
                "duplicate SQL model names detected under models/**/*.sql (dbt model-name collision):\n- {}\n\nKeep each model name in exactly one canonical path before dbt_validate.",
                lines.join("\n- ")
            ),
            suggested_targets,
        });
    }

    let key = project_fs::join_storage_key(ctx, project_fs::MODELS_SCHEMA_YML);
    let schema_map: Option<serde_yaml::Mapping> =
        match retry_get_bytes(ctx.storage().as_ref(), &key).await {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).to_string();
                let root: YamlValue = serde_yaml::from_str(&text).map_err(|e| PrecheckFailure {
                    kind: PrecheckFailureKind::Other,
                    brief: format!("models/schema.yml parse error: {e}"),
                    suggested_targets: vec![project_fs::MODELS_SCHEMA_YML.to_string()],
                })?;
                let YamlValue::Mapping(map) = root else {
                    return Err(PrecheckFailure {
                        kind: PrecheckFailureKind::Other,
                        brief: "models/schema.yml root must be a mapping".to_string(),
                        suggested_targets: vec![project_fs::MODELS_SCHEMA_YML.to_string()],
                    });
                };
                Some(map)
            }
            Err(_) => None, // missing schema.yml is fine for early projects
        };
    let staging_yml_model_names = collect_staging_model_names_from_ymls(ctx, 200)
        .await
        .unwrap_or_default();
    let mut schema_model_names: HashSet<String> = HashSet::new();
    if let Some(models) = schema_map
        .as_ref()
        .and_then(|m| m.get("models"))
        .and_then(|v| v.as_sequence())
    {
        for m in models.iter() {
            let Some(name) = m
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            if yaml_is_staging_model_name(&name) {
                return Err(PrecheckFailure {
                    kind: PrecheckFailureKind::SchemaModelMisplacement,
                    brief: format!(
                        "models/schema.yml contains staging model '{name}'. Staging model docs/tests must be in models/staging/*.yml to avoid duplicate definitions."
                    ),
                    suggested_targets: vec![
                        project_fs::MODELS_SCHEMA_YML.to_string(),
                        format!("models/staging/{name}.yml"),
                    ],
                });
            }
            schema_model_names.insert(name.clone());
            // Validate test dict shapes under model-level tests and column-level tests.
            if let Some(YamlValue::Sequence(tests)) = m.get("tests") {
                for t in tests.iter() {
                    if !test_mapping_has_single_key(t) {
                        return Err(PrecheckFailure {
                            kind: PrecheckFailureKind::InvalidTestDefinitionShape,
                            brief: "invalid test config in models/schema.yml: each test definition dictionary must have exactly one key".to_string(),
                            suggested_targets: vec![project_fs::MODELS_SCHEMA_YML.to_string()],
                        });
                    }
                }
            }
            if let Some(YamlValue::Sequence(cols)) = m.get("columns") {
                for c in cols.iter() {
                    if let Some(YamlValue::Sequence(tests)) = c.get("tests") {
                        for t in tests.iter() {
                            if !test_mapping_has_single_key(t) {
                                return Err(PrecheckFailure {
                                    kind: PrecheckFailureKind::InvalidTestDefinitionShape,
                                    brief: "invalid test config in models/schema.yml: each test definition dictionary must have exactly one key".to_string(),
                                    suggested_targets: vec![project_fs::MODELS_SCHEMA_YML.to_string()],
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    // Duplicate detection: any model defined in both locations is a dbt compile-time footgun.
    let mut dup: Vec<String> = schema_model_names
        .intersection(&staging_yml_model_names)
        .cloned()
        .collect();
    dup.sort();
    dup.dedup();
    if !dup.is_empty() {
        let mut suggested_targets = vec![project_fs::MODELS_SCHEMA_YML.to_string()];
        for name in &dup {
            suggested_targets.push(format!("models/staging/{name}.yml"));
        }
        return Err(PrecheckFailure {
            kind: PrecheckFailureKind::DuplicateModelDefinition,
            brief: format!(
                "duplicate model definitions detected in both models/schema.yml and models/staging/*.yml: {}. Keep staging docs/tests in models/staging/*.yml and gold docs/tests in models/schema.yml only.",
                dup.join(", ")
            ),
            suggested_targets,
        });
    }

    // Staging schema guards: parse each staging YAML and ensure declared columns exist
    // in the corresponding staging SQL output.
    let base = ctx
        .keyspace()
        .scoped_prefix(ctx.scope(), &["dbt"])
        .trim_end_matches('/')
        .to_string();
    let pref = format!("{}/models/staging/", base);
    let mut keys = retry_list_prefix(ctx.storage().as_ref(), &pref)
        .await
        .unwrap_or_default();
    keys.sort();
    for key in keys.into_iter().filter(|k| k.ends_with(".yml")) {
        let rel = key
            .strip_prefix(&(base.clone() + "/"))
            .unwrap_or(key.as_str())
            .to_string();
        let bytes = retry_get_bytes(ctx.storage().as_ref(), &key)
            .await
            .map_err(|e| PrecheckFailure {
                kind: PrecheckFailureKind::Other,
                brief: format!("failed to read {rel}: {e}"),
                suggested_targets: vec![rel.clone()],
            })?;
        let text = String::from_utf8_lossy(&bytes).to_string();
        let root: YamlValue = serde_yaml::from_str(&text).map_err(|e| PrecheckFailure {
            kind: PrecheckFailureKind::Other,
            brief: format!("invalid YAML in {rel}: {e}"),
            suggested_targets: vec![rel.clone()],
        })?;
        let Some(models) = root.get("models").and_then(|v| v.as_sequence()) else {
            continue;
        };
        for model in models.iter() {
            let Some(name) = model
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let declared = extract_declared_columns_from_model_entry(model);
            if declared.is_empty() {
                continue;
            }
            let sql_rel = format!("models/staging/{name}.sql");
            let sql_key = project_fs::join_storage_key(ctx, &sql_rel);
            let sql_bytes = retry_get_bytes(ctx.storage().as_ref(), &sql_key)
                .await
                .map_err(|_| PrecheckFailure {
                    kind: PrecheckFailureKind::StagingSchemaColumnMismatch,
                    brief: format!(
                        "cannot validate {rel}: missing staging SQL {sql_rel} for model '{name}'"
                    ),
                    suggested_targets: vec![rel.clone(), sql_rel.clone()],
                })?;
            let sql_text = String::from_utf8_lossy(&sql_bytes).to_string();
            let allowed = crate::tools::files_tool::extract_final_select_output_columns(&sql_text)
                .map_err(|e| PrecheckFailure {
                    kind: PrecheckFailureKind::StagingSchemaColumnMismatch,
                    brief: format!(
                        "cannot validate {rel} against {sql_rel} (model '{name}'): {}",
                        e.trim()
                    ),
                    suggested_targets: vec![rel.clone(), sql_rel.clone()],
                })?;
            let mut unknown: Vec<String> = declared
                .into_iter()
                .filter(|c| !allowed.contains(c))
                .collect();
            unknown.sort();
            unknown.dedup();
            if !unknown.is_empty() {
                return Err(PrecheckFailure {
                    kind: PrecheckFailureKind::StagingSchemaColumnMismatch,
                    brief: format!(
                        "staging schema references unknown columns for model '{name}'.\nFile: {rel}\nStaging SQL: {sql_rel}\nUnknown columns:\n- {}\n\nFix: update staging SQL outputs or remove/rename these YAML columns before dbt_validate.",
                        unknown.join("\n- ")
                    ),
                    suggested_targets: vec![rel.clone(), sql_rel.clone()],
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::{ChatMessage, LargeLanguageModel};
    use react_core::scope::RequestScope;
    use react_core::storage::StorageAdapter;
    use react_module_storage_memory::InMemoryStorageAdapter;
    use std::sync::Arc;

    #[derive(Default)]
    struct DummyLlm;
    impl LargeLanguageModel for DummyLlm {
        fn chat(
            &self,
            _messages: &[ChatMessage],
            _options: &react_core::llm::LlmCallOptions,
        ) -> Result<String, String> {
            Err("dummy".to_string())
        }
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
    }

    fn make_ctx(storage: Arc<dyn StorageAdapter>) -> AgentCtx {
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        react_core::agent::AgentCtxBuilder::new(
            Arc::new(DummyLlm::default()),
            storage,
            RequestScope::parse("t", "w", "p").expect("valid test scope"),
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(2)
        .thread_id("t".to_string())
        .agent_name("test".to_string())
        .build()
    }

    #[test]
    fn sanitize_models_schema_yml_removes_stg_models() {
        let yml = "version: 2\nmodels:\n  - name: stg_test_raw_raw_customers\n    columns: []\n  - name: dim_customers\n    columns: []\n";
        let touched: HashSet<String> = HashSet::from(["dim_customers".to_string()]);
        let allowed: HashMap<String, ModelAllowedColumns> = HashMap::new();
        let (out, warnings) = sanitize_models_schema_yml(yml, &touched, &allowed).expect("ok");
        assert!(!out.contains("stg_test_raw_raw_customers"));
        assert!(out.contains("dim_customers"));
        assert!(warnings.iter().any(|w| w.contains("removed staging model")));
    }

    #[tokio::test]
    async fn prevalidate_detects_duplicate_model_names_between_schema_and_staging_yml() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage.clone());

        let schema_key = project_fs::join_storage_key(&ctx, project_fs::MODELS_SCHEMA_YML);
        ctx.storage()
            .put_bytes(
                &schema_key,
                b"version: 2\nmodels:\n  - name: dim_customers\n    columns: []\n",
                "text/yaml",
            )
            .await
            .unwrap();

        let stg_key =
            project_fs::join_storage_key(&ctx, "models/staging/stg_test_raw_raw_customers.yml");
        ctx.storage()
            .put_bytes(
                &stg_key,
                b"version: 2\nmodels:\n  - name: dim_customers\n    columns: []\n",
                "text/yaml",
            )
            .await
            .unwrap();

        let err = prevalidate_dbt_schema_artifacts(&ctx).await.unwrap_err();
        assert_eq!(err.kind, PrecheckFailureKind::DuplicateModelDefinition);
        assert!(err.brief.contains("duplicate model definitions"));
        assert!(err
            .suggested_targets
            .iter()
            .any(|t| t == project_fs::MODELS_SCHEMA_YML));
    }

    #[tokio::test]
    async fn prevalidate_returns_schema_misplacement_for_stg_in_schema_yml() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage.clone());

        let schema_key = project_fs::join_storage_key(&ctx, project_fs::MODELS_SCHEMA_YML);
        ctx.storage()
            .put_bytes(
                &schema_key,
                b"version: 2\nmodels:\n  - name: stg_picnic_screen_birthdate\n    columns: []\n",
                "text/yaml",
            )
            .await
            .unwrap();

        let err = prevalidate_dbt_schema_artifacts(&ctx).await.unwrap_err();
        assert_eq!(err.kind, PrecheckFailureKind::SchemaModelMisplacement);
        assert!(err.brief.contains("stg_picnic_screen_birthdate"));
        assert!(err
            .suggested_targets
            .iter()
            .any(|t| t == "models/staging/stg_picnic_screen_birthdate.yml"));
    }

    #[tokio::test]
    async fn normalize_schema_artifacts_merges_duplicate_model_entries() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage.clone());
        let schema_key = project_fs::join_storage_key(&ctx, project_fs::MODELS_SCHEMA_YML);
        ctx.storage()
            .put_bytes(
                &schema_key,
                b"version: 2\nmodels:\n  - name: dim_orders\n    tests:\n      - not_null\n    columns:\n      - name: order_id\n        tests:\n          - not_null\n  - name: dim_orders\n    tests:\n      - not_null\n    columns:\n      - name: order_id\n        tests:\n          - not_null\n      - name: customer_id\n        tests:\n          - not_null\n",
                "text/yaml",
            )
            .await
            .unwrap();

        let notes = normalize_schema_artifacts_for_validate(&ctx)
            .await
            .expect("normalize ok");
        assert!(!notes.is_empty());

        let got = String::from_utf8_lossy(&ctx.storage().get_bytes(&schema_key).await.unwrap())
            .to_string();
        // Only one model stanza remains.
        assert_eq!(got.matches("name: dim_orders").count(), 1);
        assert!(got.contains("customer_id"));
    }

    #[tokio::test]
    async fn prevalidate_detects_unknown_columns_in_staging_yml_against_sql() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage.clone());

        let sql_key =
            project_fs::join_storage_key(&ctx, "models/staging/stg_test_raw_raw_orders.sql");
        ctx.storage()
            .put_bytes(
                &sql_key,
                b"select 1 as order_id, 'x' as placed_at_raw",
                "text/sql",
            )
            .await
            .unwrap();
        let yml_key =
            project_fs::join_storage_key(&ctx, "models/staging/stg_test_raw_raw_orders.yml");
        ctx.storage()
            .put_bytes(
                &yml_key,
                b"version: 2\nmodels:\n  - name: stg_test_raw_raw_orders\n    columns:\n      - name: order_id\n      - name: missing_col\n",
                "text/yaml",
            )
            .await
            .unwrap();

        let err = prevalidate_dbt_schema_artifacts(&ctx).await.unwrap_err();
        assert_eq!(err.kind, PrecheckFailureKind::StagingSchemaColumnMismatch);
        assert!(err.brief.contains("stg_test_raw_raw_orders"));
        assert!(err.brief.contains("models/staging/stg_test_raw_raw_orders"));
    }

    #[tokio::test]
    async fn prevalidate_detects_duplicate_sql_model_stems() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage.clone());

        let marts_key = project_fs::join_storage_key(&ctx, "models/marts/fct_orders.sql");
        ctx.storage()
            .put_bytes(&marts_key, b"select 1 as id", "text/sql")
            .await
            .unwrap();
        let core_key = project_fs::join_storage_key(&ctx, "models/core/fct_orders.sql");
        ctx.storage()
            .put_bytes(&core_key, b"select 2 as id", "text/sql")
            .await
            .unwrap();

        let err = prevalidate_dbt_schema_artifacts(&ctx).await.unwrap_err();
        assert_eq!(err.kind, PrecheckFailureKind::DuplicateSqlModelStem);
        assert!(err.brief.contains("duplicate SQL model names"));
        assert!(err.brief.contains("fct_orders"));
        assert!(err.brief.contains("models/marts/fct_orders.sql"));
        assert!(err.brief.contains("models/core/fct_orders.sql"));
        assert!(err
            .suggested_targets
            .iter()
            .any(|t| t == "models/marts/fct_orders.sql"));
    }
}
