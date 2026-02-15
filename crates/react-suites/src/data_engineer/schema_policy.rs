use serde_yaml::Value as YamlValue;
use std::collections::{HashMap, HashSet};

use react_core::agent::AgentCtx;

use crate::data_engineer::project_files;
use crate::data_engineer::project_fs;

#[derive(Clone, Debug, Default)]
pub struct ModelAllowedColumns {
    /// If empty, we cannot safely author tests; docs-only changes are allowed.
    pub allowed_columns: HashSet<String>,
    pub error: Option<String>,
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
    name.trim_start().starts_with("stg_")
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

async fn collect_staging_model_names_from_ymls(
    ctx: &AgentCtx,
    limit: usize,
) -> Result<HashSet<String>, String> {
    let limit = limit.max(1).min(500);
    let base = ctx
        .keyspace
        .dbt_prefix(&ctx.scope)
        .trim_end_matches('/')
        .to_string();
    let pref = format!("{}/models/staging/", base);
    let mut keys = ctx.storage.list_prefix(&pref).await.unwrap_or_default();
    keys.sort();
    let mut out: HashSet<String> = HashSet::new();
    for k in keys.into_iter().filter(|k| k.ends_with(".yml")).take(limit) {
        let bytes = match ctx.storage.get_bytes(&k).await {
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
    let YamlValue::Mapping(m) = v else { return None };
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
    let YamlValue::Mapping(m) = v else { return None };
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
        let Some(_test_name) = mapping_single_key_name(it) else { return true };
        let Some(cfg) = mapping_single_key_value(it) else { return true };
        let YamlValue::Mapping(cfgm) = cfg else { return true };
        let Some(where_v) = cfgm.get("where") else { return true };
        let Some(where_s) = where_v.as_str() else { return true };
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
    let models_v = map.entry(models_key).or_insert_with(|| YamlValue::Sequence(vec![]));
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
            warnings.push(format!("{name}: allowed_columns unavailable ({err}); tests removed"));
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
                let YamlValue::Mapping(ref mut cm) = c else { continue };
                let col_name = yaml_get_str(&YamlValue::Mapping(cm.clone()), "name")
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let col_is_allowed = !col_name.is_empty() && allowed.allowed_columns.contains(&col_name);
                if let Some(YamlValue::Sequence(ref mut tests)) = cm.get_mut("tests") {
                    if forbid_tests || !col_is_allowed {
                        if !tests.is_empty() {
                            warnings.push(format!(
                                "{name}.{col_name}: removed tests (column not in allowed_columns or allowed_columns unavailable)"
                            ));
                        }
                        tests.clear();
                    } else {
                        warnings.extend(sanitize_tests_seq(
                            tests,
                            &allowed.allowed_columns,
                            false,
                        ));
                    }
                }
            }
        }

        out_models.push(YamlValue::Mapping(mm));
    }
    *models_seq = out_models;

    let out = serde_yaml::to_string(&root).map_err(|e| format!("failed to render schema.yml: {e}"))?;
    Ok((out, warnings))
}

/// Cheap structural prechecks to avoid burning dbt_validate cycles on trivial YAML issues.
///
/// Checks:
/// - `models/schema.yml` parses (if present)
/// - No `stg_*` models are defined in `models/schema.yml`
/// - All test dicts under any `tests:` list are single-key mappings
pub async fn prevalidate_dbt_schema_artifacts(ctx: &AgentCtx) -> Result<(), String> {
    let key = project_fs::join_storage_key(ctx, project_files::MODELS_SCHEMA_YML);
    let bytes = match ctx.storage.get_bytes(&key).await {
        Ok(b) => b,
        Err(_) => return Ok(()), // missing schema.yml is fine for early projects
    };
    let text = String::from_utf8_lossy(&bytes).to_string();
    let root: YamlValue =
        serde_yaml::from_str(&text).map_err(|e| format!("models/schema.yml parse error: {e}"))?;
    let YamlValue::Mapping(map) = root else {
        return Err("models/schema.yml root must be a mapping".to_string());
    };
    let staging_yml_model_names =
        collect_staging_model_names_from_ymls(ctx, 200).await.unwrap_or_default();
    let mut schema_model_names: HashSet<String> = HashSet::new();
    if let Some(YamlValue::Sequence(models)) = map.get("models") {
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
                return Err(format!(
                    "models/schema.yml contains staging model '{name}'. Staging model docs/tests must be in models/staging/*.yml to avoid duplicate definitions."
                ));
            }
            schema_model_names.insert(name.clone());
            // Validate test dict shapes under model-level tests and column-level tests.
            if let Some(YamlValue::Sequence(tests)) = m.get("tests") {
                for t in tests.iter() {
                    if !test_mapping_has_single_key(t) {
                        return Err("invalid test config in models/schema.yml: each test definition dictionary must have exactly one key".to_string());
                    }
                }
            }
            if let Some(YamlValue::Sequence(cols)) = m.get("columns") {
                for c in cols.iter() {
                    if let Some(YamlValue::Sequence(tests)) = c.get("tests") {
                        for t in tests.iter() {
                            if !test_mapping_has_single_key(t) {
                                return Err("invalid test config in models/schema.yml: each test definition dictionary must have exactly one key".to_string());
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
        return Err(format!(
            "duplicate model definitions detected in both models/schema.yml and models/staging/*.yml: {}. Keep staging docs/tests in models/staging/*.yml and gold docs/tests in models/schema.yml only.",
            dup.join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::{ChatMessage, LargeLanguageModel};
    use react_core::scope::RequestScope;
    use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};
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
        AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 2,
            thread_id: Some("t".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm: Arc::new(DummyLlm::default()),
            storage,
            scope: RequestScope {
                tenant: "t".to_string(),
                workspace: "w".to_string(),
                project_id: "p".to_string(),
            },
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            runtime: None,
        }
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

        let schema_key = project_fs::join_storage_key(&ctx, project_files::MODELS_SCHEMA_YML);
        ctx.storage
            .put_bytes(
                &schema_key,
                b"version: 2\nmodels:\n  - name: dim_customers\n    columns: []\n",
                "text/yaml",
            )
            .await
            .unwrap();

        let stg_key = project_fs::join_storage_key(&ctx, "models/staging/stg_test_raw_raw_customers.yml");
        ctx.storage
            .put_bytes(
                &stg_key,
                b"version: 2\nmodels:\n  - name: dim_customers\n    columns: []\n",
                "text/yaml",
            )
            .await
            .unwrap();

        let err = prevalidate_dbt_schema_artifacts(&ctx).await.unwrap_err();
        assert!(err.contains("duplicate model definitions"));
    }
}

