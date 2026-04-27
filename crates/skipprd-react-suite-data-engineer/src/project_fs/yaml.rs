use serde_yaml::{Mapping as YamlMapping, Value as YamlValue};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use crate::naming;
use crate::providers::DatasetCatalogProvider;
use react_core::agent::AgentCtx;
use react_core::storage::{retry_get_bytes, retry_list_prefix};

pub const PACKAGES_YML: &str = "packages.yml";
pub const MODELS_SCHEMA_YML: &str = "models/schema.yml";

pub(crate) async fn postprocess_content(
    ctx: &AgentCtx,
    datasets: Option<&Arc<dyn DatasetCatalogProvider>>,
    rel: &str,
    content: &str,
) -> Result<String, String> {
    if rel == PACKAGES_YML {
        return postprocess_packages_yml(content);
    }
    if rel == MODELS_SCHEMA_YML {
        return postprocess_schema_yml(ctx, datasets, content).await;
    }
    if rel.starts_with("models/staging/") && rel.ends_with(".yml") {
        return disable_contract_enforcement_in_schema_yml_text(content);
    }
    if rel.starts_with("models/") && rel.ends_with(".sql") {
        return postprocess_model_sql(ctx, rel, content);
    }
    Ok(content.to_string())
}

fn disable_contract_enforcement_in_schema_yml_text(yml_text: &str) -> Result<String, String> {
    let mut root: serde_yaml::Value =
        serde_yaml::from_str(yml_text).map_err(|e| format!("invalid YAML: {e}"))?;
    let Some(models) = root
        .as_mapping_mut()
        .and_then(|m| m.get_mut(serde_yaml::Value::String("models".to_string())))
        .and_then(|v| v.as_sequence_mut())
    else {
        return Ok(yml_text.to_string());
    };

    for m in models.iter_mut() {
        let Some(mm) = m.as_mapping_mut() else {
            continue;
        };
        let cfg = mm
            .entry(serde_yaml::Value::String("config".to_string()))
            .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
        let Some(cfgm) = cfg.as_mapping_mut() else {
            continue;
        };
        let contract = cfgm
            .entry(serde_yaml::Value::String("contract".to_string()))
            .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
        let Some(cm) = contract.as_mapping_mut() else {
            continue;
        };
        cm.insert(
            serde_yaml::Value::String("enforced".to_string()),
            serde_yaml::Value::Bool(false),
        );
    }

    serde_yaml::to_string(&root)
        .map_err(|e| format!("failed to re-serialize YAML: {}", e.to_string()))
        .map(|s| s.trim_start_matches("---\n").to_string())
}

/// Canonicalize `models/schema.yml` content.
///
/// This is used when we want deterministic schema.yml normalization (e.g. rebuilding sources from
/// the dataset catalog) without requiring the caller to go through a patch-apply cycle.
pub async fn canonicalize_schema_yml(
    ctx: &AgentCtx,
    datasets: Option<&Arc<dyn DatasetCatalogProvider>>,
    content: &str,
) -> Result<String, String> {
    postprocess_schema_yml(ctx, datasets, content).await
}

async fn postprocess_schema_yml(
    ctx: &AgentCtx,
    datasets: Option<&Arc<dyn DatasetCatalogProvider>>,
    content: &str,
) -> Result<String, String> {
    let wh = crate::ctx_ext::actx_warehouse(ctx)
        .ok_or_else(|| "warehouse provider missing for schema.yml postprocess".to_string())?;
    let q = wh.as_ref();
    let cfg = crate::resolved_config_from_ctx(ctx)
        .ok_or_else(|| "resolved_config missing for schema.yml postprocess".to_string())?;
    let providers = crate::de_config::de_config_from_resolved(cfg)
        .ok_or_else(|| "suite_config missing or invalid for schema.yml postprocess".to_string())?;
    let want_catalog = providers.warehouse.container.clone();
    let want_schema = providers.warehouse.namespace.clone();
    const MAX_PROVED_SOURCES: usize = 200;

    fn parse_sources_from_schema_yml(root: &YamlMapping) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let Some(YamlValue::Sequence(srcs)) = root.get(&YamlValue::String("sources".to_string()))
        else {
            return out;
        };
        for src in srcs.iter() {
            let Some(m) = src.as_mapping() else { continue };
            let name = m
                .get(&YamlValue::String("name".to_string()))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let tables = m
                .get(&YamlValue::String("tables".to_string()))
                .and_then(|v| v.as_sequence())
                .cloned()
                .unwrap_or_default();
            for t in tables.into_iter() {
                let Some(tm) = t.as_mapping() else { continue };
                let tn = tm
                    .get(&YamlValue::String("name".to_string()))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !name.is_empty() && !tn.is_empty() {
                    out.push((name.clone(), tn));
                }
            }
        }
        out
    }

    fn sources_value_from_fqns(fqns: &std::collections::BTreeSet<String>) -> YamlValue {
        let grouped = crate::dataset_truth::group_by_catalog_schema(fqns);
        let mut sources_seq: Vec<YamlValue> = Vec::new();
        for ((cat, db), mut tables) in grouped.into_iter() {
            tables.sort();
            tables.dedup();
            let mut src = YamlMapping::new();
            src.insert(
                YamlValue::String("name".to_string()),
                YamlValue::String(db.clone()),
            );
            src.insert(
                YamlValue::String("database".to_string()),
                YamlValue::String(cat),
            );
            src.insert(
                YamlValue::String("schema".to_string()),
                YamlValue::String(db),
            );
            let mut tables_seq: Vec<YamlValue> = Vec::new();
            for t in tables.into_iter() {
                let mut tm = YamlMapping::new();
                tm.insert(YamlValue::String("name".to_string()), YamlValue::String(t));
                tables_seq.push(YamlValue::Mapping(tm));
            }
            src.insert(
                YamlValue::String("tables".to_string()),
                YamlValue::Sequence(tables_seq),
            );
            sources_seq.push(YamlValue::Mapping(src));
        }
        YamlValue::Sequence(sources_seq)
    }

    let mut root = if content.trim().is_empty() {
        YamlMapping::new()
    } else {
        let v: YamlValue =
            serde_yaml::from_str(content).map_err(|e| format!("schema.yml parse error: {}", e))?;
        match v {
            YamlValue::Mapping(m) => m,
            _ => return Err("models/schema.yml must be a YAML mapping at top level".to_string()),
        }
    };

    if !root.contains_key(&YamlValue::String("version".to_string())) {
        root.insert(
            YamlValue::String("version".to_string()),
            YamlValue::Number(2.into()),
        );
    }

    let mut candidates: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for (schema, table) in parse_sources_from_schema_yml(&root).into_iter() {
        let s = schema.trim().to_string();
        let t = table.trim().to_string();
        if s == want_schema && !t.is_empty() {
            candidates.insert(format!("{}.{}.{}", want_catalog, s, t));
        }
    }

    {
        let base = ctx
            .keyspace()
            .scoped_prefix(ctx.scope(), &["dbt"])
            .trim_end_matches('/')
            .to_string()
            + "/";
        let staging_prefix = format!("{}models/staging/", base);
        if let Ok(keys) = retry_list_prefix(ctx.storage().as_ref(), &staging_prefix).await {
            for k in keys {
                if !k.ends_with(".sql") || k.contains("/_versions/") {
                    continue;
                }
                if let Ok(bytes) = retry_get_bytes(ctx.storage().as_ref(), &k).await {
                    let sql = String::from_utf8_lossy(&bytes).to_string();
                    for (schema, table) in crate::naming::extract_source_calls(&sql).into_iter() {
                        if schema == want_schema && !table.trim().is_empty() {
                            candidates.insert(format!("{}.{}.{}", want_catalog, schema, table));
                        }
                    }
                }
            }
        }
    }

    if let Some(ds) = datasets {
        if let Ok(listed) = ds.list_datasets().await {
            for d in listed.into_iter().take(MAX_PROVED_SOURCES) {
                if d.catalog == want_catalog && d.database == want_schema {
                    candidates.insert(d.fqn());
                }
            }
        }
    }

    let mut proven: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for fqn in candidates.into_iter() {
        if proven.len() >= MAX_PROVED_SOURCES {
            break;
        }
        if let Ok(_cols) =
            crate::transient_retry::retry_transient_default("yaml_source_prove", || async {
                q.schema(&fqn).await
            })
            .await
        {
            proven.insert(fqn);
        }
    }

    let sources_val = sources_value_from_fqns(&proven);
    root.insert(YamlValue::String("sources".to_string()), sources_val);
    serde_yaml::to_string(&YamlValue::Mapping(root)).map_err(|e| e.to_string())
}

fn postprocess_packages_yml(content: &str) -> Result<String, String> {
    let mut root = if content.trim().is_empty() {
        YamlMapping::new()
    } else {
        let v: YamlValue = serde_yaml::from_str(content)
            .map_err(|e| format!("packages.yml parse error: {}", e))?;
        match v {
            YamlValue::Mapping(m) => m,
            _ => return Err("packages.yml must be a YAML mapping at top level".to_string()),
        }
    };

    let packages_seq = match root.get(&YamlValue::String("packages".to_string())) {
        Some(YamlValue::Sequence(seq)) => seq.clone(),
        Some(_) => return Err("packages.yml 'packages' must be a list".to_string()),
        None => Vec::new(),
    };

    let mut by_key: BTreeMap<String, YamlValue> = BTreeMap::new();
    for item in packages_seq.into_iter() {
        let YamlValue::Mapping(m) = item else {
            continue;
        };
        let key = package_entry_key(&m).unwrap_or_else(|| format!("unknown:{}", by_key.len()));
        let canonical = canonicalize_package_entry(&m);
        by_key.insert(key, YamlValue::Mapping(canonical));
    }

    let normalized: Vec<YamlValue> = by_key.into_values().collect();
    root.insert(
        YamlValue::String("packages".to_string()),
        YamlValue::Sequence(normalized),
    );
    serde_yaml::to_string(&YamlValue::Mapping(root)).map_err(|e| e.to_string())
}

fn package_entry_key(m: &YamlMapping) -> Option<String> {
    let package = yaml_string_value(m, "package");
    let git = yaml_string_value(m, "git");
    let local = yaml_string_value(m, "local");
    if let Some(p) = package {
        return Some(format!("package:{}", p));
    }
    if let Some(g) = git {
        return Some(format!("git:{}", g));
    }
    if let Some(l) = local {
        return Some(format!("local:{}", l));
    }
    None
}

fn canonicalize_package_entry(m: &YamlMapping) -> YamlMapping {
    let mut out = YamlMapping::new();
    let package = yaml_string_value(m, "package");
    let git = yaml_string_value(m, "git");
    let local = yaml_string_value(m, "local");
    if let Some(p) = package {
        out.insert(
            YamlValue::String("package".to_string()),
            YamlValue::String(p),
        );
    } else if let Some(g) = git {
        out.insert(YamlValue::String("git".to_string()), YamlValue::String(g));
    } else if let Some(l) = local {
        out.insert(YamlValue::String("local".to_string()), YamlValue::String(l));
    }
    insert_if_present(&mut out, m, "version");
    insert_if_present(&mut out, m, "revision");
    insert_if_present(&mut out, m, "subdir");

    let mut extra: BTreeMap<String, YamlValue> = BTreeMap::new();
    for (k, v) in m {
        let Some(ks) = k.as_str() else { continue };
        if ks == "package"
            || ks == "git"
            || ks == "local"
            || ks == "version"
            || ks == "revision"
            || ks == "subdir"
        {
            continue;
        }
        extra.insert(ks.to_string(), v.clone());
    }
    for (k, v) in extra {
        out.insert(YamlValue::String(k), v);
    }
    out
}

fn insert_if_present(out: &mut YamlMapping, src: &YamlMapping, key: &str) {
    let key_val = YamlValue::String(key.to_string());
    if let Some(v) = src.get(&key_val) {
        out.insert(key_val, v.clone());
    }
}

fn yaml_string_value(m: &YamlMapping, key: &str) -> Option<String> {
    m.get(&YamlValue::String(key.to_string()))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn postprocess_model_sql(ctx: &AgentCtx, rel: &str, content: &str) -> Result<String, String> {
    validate_model_sql_identity(rel, content)?;
    let cfg = crate::resolved_config_from_ctx(ctx)
        .ok_or_else(|| "resolved_config missing for model SQL postprocess".to_string())?;
    let providers = crate::de_config::de_config_from_resolved(cfg)
        .ok_or_else(|| "suite_config missing or invalid for model SQL postprocess".to_string())?;
    let suffix = tier_suffix_for_path(rel, &providers)
        .ok_or_else(|| "unable to infer tier suffix for model path".to_string())?;
    let alias = model_alias_from_rel(rel)
        .ok_or_else(|| "unable to infer model alias from path".to_string())?;
    Ok(rewrite_config_header(content, &suffix, &alias))
}

fn validate_model_sql_identity(rel: &str, content: &str) -> Result<(), String> {
    if !rel.starts_with("models/") || !rel.ends_with(".sql") {
        return Ok(());
    }

    if rel.starts_with("models/staging/") {
        let sources = naming::extract_source_calls(content);
        if sources.is_empty() {
            return Err(format!(
                "invalid silver model SQL at '{}': silver models under models/staging/ must contain exactly one dbt source() call and be written to the canonical path models/staging/stg_<source_schema>_<source_table>.sql",
                rel
            ));
        }
        if sources.len() != 1 {
            return Err(format!(
                "invalid silver model SQL at '{}': silver models under models/staging/ must reference exactly ONE source(schema, table). Found: {:?}",
                rel, sources
            ));
        }
        let (schema, table) = &sources[0];
        let canonical = naming::canonical_staging_rel_path(schema, table);
        if rel != canonical {
            return Err(format!(
                "invalid silver model path: silver model for source(\"{}\",\"{}\") must be written to '{}' (canonical), but attempted to write '{}'. Rename the file to the canonical path (no alternate naming schemes are permitted).",
                schema, table, canonical, rel
            ));
        }
        return Ok(());
    }

    let sources = naming::extract_source_calls(content);
    if !sources.is_empty() {
        return Err(format!(
            "invalid gold/core model SQL at '{}': gold models must NOT reference dbt source() (raw/bronze). Use ref() to read from silver or other gold models. Found source() call(s): {:?}",
            rel, sources
        ));
    }
    Ok(())
}

fn tier_suffix_for_path(rel: &str, pcfg: &crate::de_config::ProvidersResolved) -> Option<String> {
    if rel.starts_with("models/staging/") {
        return Some(pcfg.dbt.naming.silver_suffix.clone());
    }
    if rel.starts_with("models/core/") || rel.starts_with("models/marts/") {
        return Some(pcfg.dbt.naming.gold_suffix.clone());
    }
    if rel.starts_with("models/") {
        return Some(pcfg.dbt.naming.gold_suffix.clone());
    }
    None
}

fn model_alias_from_rel(rel: &str) -> Option<String> {
    let fname = Path::new(rel).file_name()?.to_string_lossy();
    let stem = fname.trim_end_matches(".sql");
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

fn rewrite_config_header(content: &str, _schema_suffix: &str, alias: &str) -> String {
    let mut body_lines: Vec<String> = Vec::new();
    for line in content.lines() {
        let t = line.trim();
        let is_config = (t.starts_with("{{") || t.starts_with("{%")) && t.contains("config(");
        if is_config {
            continue;
        }
        body_lines.push(line.to_string());
    }
    let body = body_lines.join("\n").trim_start().to_string();
    if body.is_empty() {
        return format!("{{{{ config(alias=\"{}\") }}}}\n", alias);
    }
    format!("{{{{ config(alias=\"{}\") }}}}\n\n{}", alias, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_fs::test_helpers::*;

    #[tokio::test]
    async fn schema_yml_filters_unproven_sources_via_schema_facts() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(react_module_storage_memory::InMemoryStorageAdapter::default());
        let q = MockQuery::default();
        *q.schemas.lock().unwrap() = std::collections::HashMap::from([(
            "AwsDataCatalog.test_raw.raw_customers".to_string(),
            vec![("id".to_string(), "varchar".to_string())],
        )]);
        let warehouse: Arc<dyn crate::providers::WarehouseProvider> = Arc::new(MockWarehouse {
            schemas: q.schemas.clone(),
        });
        let mut ctx = make_ctx(storage, Some(Arc::new(q)));
        ctx.set_capability(Arc::new(crate::ctx_ext::WarehouseCap(warehouse)));
        let datasets: Arc<dyn DatasetCatalogProvider> = Arc::new(MockDatasets { items: vec![] });

        let existing = r#"
version: 2
sources:
  - name: test_raw
    database: AwsDataCatalog
    schema: test_raw
    tables:
      - name: raw_customers
      - name: raw_products
"#;
        let out = canonicalize_schema_yml(&ctx, Some(&datasets), existing)
            .await
            .expect("ok");
        assert!(out.contains("raw_customers"));
        assert!(!out.contains("raw_products"));
    }
}
