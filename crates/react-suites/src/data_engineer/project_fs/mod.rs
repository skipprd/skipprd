use serde_json::Value;
use serde_yaml::{Mapping as YamlMapping, Value as YamlValue};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

use diffy::Patch;
use react_core::agent::AgentCtx;
use react_core::providers::{DatasetCatalogProvider, DatasetId};

#[derive(Debug)]
pub struct PatchOutcome {
    pub rel_path: String,
    pub key: String,
    pub existed: bool,
    pub base_sha256: String,
    pub new_sha256: String,
    pub diff: String,
    pub lines_added: usize,
    pub lines_removed: usize,
    pub content: String,
}

pub fn create_patch_text(old: &str, new: &str) -> String {
    diffy::create_patch(old, new).to_string()
}

pub fn normalize_rel_path(rel: &str) -> Result<String, String> {
    let rel = rel.trim().trim_start_matches("./").to_string();
    if !is_allowed_rel_path(&rel) {
        return Err("path not allowed; only dbt project files under models/, seeds/, macros/, snapshots/, analyses/, tests/, target/ (or dbt_project.yml / packages.yml) are permitted".to_string());
    }
    Ok(rel.replace('\\', "/"))
}

pub fn join_storage_key(ctx: &AgentCtx, rel: &str) -> String {
    let base = ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string();
    format!("{}/{}", base, rel)
}

pub async fn list_files(ctx: &AgentCtx, prefix: &str, limit: usize) -> Result<Value, String> {
    let rel_prefix = if prefix.trim().is_empty() {
        "models/".to_string()
    } else {
        normalize_rel_path(prefix)?
    };
    let key_prefix = join_storage_key(ctx, &rel_prefix.trim_start_matches('/'));
    let mut keys = ctx.storage.list_prefix(&key_prefix).await.unwrap_or_default();
    keys.sort();
    let mut out: Vec<Value> = Vec::new();
    for k in keys.into_iter().take(limit) {
        let rel = k
            .strip_prefix(&(ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string() + "/"))
            .unwrap_or(&k)
            .to_string();
        out.push(serde_json::json!({"path": rel, "key": k}));
    }
    Ok(serde_json::json!({"ok": true, "items": out}))
}

pub async fn get_file(ctx: &AgentCtx, path: &str, max_chars: usize) -> Result<Value, String> {
    let rel = normalize_rel_path(path)?;
    let key = join_storage_key(ctx, &rel);
    match ctx.storage.get_bytes(&key).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes).to_string();
            let content = if max_chars > 0 && text.len() > max_chars {
                let mut s = text.chars().take(max_chars).collect::<String>();
                s.push_str("\n... (truncated; use dbt_files op=get_json or op=manifest_find for structured access)\n");
                s
            } else {
                text
            };
            Ok(serde_json::json!({"ok": true, "path": rel, "key": key, "content": content}))
        }
        Err(e) => Err(format!("not found or failed to fetch: {}", e)),
    }
}

pub async fn get_json(ctx: &AgentCtx, path: &str, pointer: Option<&str>) -> Result<Value, String> {
    let rel = normalize_rel_path(path)?;
    let key = join_storage_key(ctx, &rel);
    let bytes = match ctx.storage.get_bytes(&key).await {
        Ok(b) => b,
        Err(e) => {
            return Ok(serde_json::json!({"ok": false, "path": rel, "key": key, "error": format!("not found or failed to fetch: {}", e)}))
        }
    };
    let v: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return Ok(serde_json::json!({"ok": false, "path": rel, "key": key, "error": format!("failed to parse json: {}", e)}));
        }
    };
    if let Some(ptr) = pointer {
        let ptr = ptr.trim();
        if ptr.is_empty() {
            return Ok(serde_json::json!({"ok": true, "path": rel, "key": key, "json": v}));
        }
        if let Some(sub) = v.pointer(ptr) {
            return Ok(serde_json::json!({"ok": true, "path": rel, "key": key, "pointer": ptr, "json": sub}));
        }
        return Ok(serde_json::json!({"ok": false, "path": rel, "key": key, "pointer": ptr, "error": "pointer not found"}));
    }
    Ok(serde_json::json!({"ok": true, "path": rel, "key": key, "json": v}))
}

pub async fn manifest_find(ctx: &AgentCtx, path: &str, unique_id: Option<&str>, name: Option<&str>, resource_type: Option<&str>, limit: usize) -> Result<Value, String> {
    let rel = normalize_rel_path(path)?;
    let key = join_storage_key(ctx, &rel);
    let bytes = match ctx.storage.get_bytes(&key).await {
        Ok(b) => b,
        Err(e) => {
            return Ok(serde_json::json!({"ok": false, "path": rel, "key": key, "error": format!("not found or failed to fetch: {}", e)}))
        }
    };
    let v: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return Ok(serde_json::json!({"ok": false, "path": rel, "key": key, "error": format!("failed to parse json: {}", e)}));
        }
    };
    let nodes = match v.get("nodes").and_then(|n| n.as_object()) {
        Some(n) => n,
        None => {
            return Ok(serde_json::json!({"ok": false, "path": rel, "key": key, "error": "manifest missing nodes"}))
        }
    };

    let mut out: Vec<serde_json::Value> = Vec::new();
    for (uid, node) in nodes.iter() {
        if let Some(ref u) = unique_id {
            if uid != u {
                continue;
            }
        }
        if let Some(ref n) = name {
            let node_name = node.get("name").and_then(|x| x.as_str()).unwrap_or("");
            if node_name != *n {
                continue;
            }
        }
        if let Some(ref rt) = resource_type {
            let node_rt = node.get("resource_type").and_then(|x| x.as_str()).unwrap_or("");
            if node_rt != *rt {
                continue;
            }
        }
        let mut slim = serde_json::Map::new();
        slim.insert("unique_id".to_string(), serde_json::json!(uid));
        for k in ["resource_type", "name", "original_file_path", "path", "package_name", "database", "schema", "alias"].iter() {
            if let Some(vv) = node.get(*k) {
                slim.insert((*k).to_string(), vv.clone());
            }
        }
        if let Some(dep) = node.get("depends_on") {
            slim.insert("depends_on".to_string(), dep.clone());
        }
        out.push(Value::Object(slim));
        if out.len() >= limit {
            break;
        }
    }
    Ok(serde_json::json!({"ok": true, "path": rel, "key": key, "items": out}))
}

pub async fn apply_patch(
    ctx: &AgentCtx,
    datasets: Option<&std::sync::Arc<dyn DatasetCatalogProvider>>,
    path: &str,
    patch_text: &str,
    base_sha256: Option<&str>,
    create_if_missing: bool,
) -> Result<PatchOutcome, String> {
    let rel = normalize_rel_path(path)?;
    let key = join_storage_key(ctx, &rel);
    let existing = ctx.storage.get_bytes(&key).await.ok().map(|b| String::from_utf8_lossy(&b).to_string());
    let existed = existing.is_some();
    if !existed && !create_if_missing {
        return Err("file does not exist; set create_if_missing=true to create via patch".to_string());
    }
    let old = existing.unwrap_or_default();
    let base_hash = sha256_hex(&old);
    if let Some(expected) = base_sha256 {
        if expected != base_hash {
            return Err(format!("base_sha256 mismatch; expected {}, got {}", expected, base_hash));
        }
    }
    let patch = Patch::from_str(patch_text).map_err(|e| format!("invalid patch: {}", e))?;
    let mut new_content = diffy::apply(&old, &patch).map_err(|e| format!("patch apply failed: {}", e))?;
    new_content = postprocess_content(ctx, datasets, &rel, &new_content).await?;

    let diff = compute_unified_diff(&old, &new_content);
    let (lines_added, lines_removed) = diff_stats(&old, &new_content);
    let new_hash = sha256_hex(&new_content);
    Ok(PatchOutcome {
        rel_path: rel,
        key,
        existed,
        base_sha256: base_hash,
        new_sha256: new_hash,
        diff,
        lines_added,
        lines_removed,
        content: new_content,
    })
}

pub fn compute_unified_diff(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    out.push("--- original".to_string());
    out.push("+++ modified".to_string());
    let mut i = 0usize;
    let mut j = 0usize;
    while i < old_lines.len() || j < new_lines.len() {
        if i < old_lines.len() && j < new_lines.len() {
            if old_lines[i] == new_lines[j] {
                out.push(format!(" {}", old_lines[i]));
                i += 1;
                j += 1;
            } else {
                out.push(format!("- {}", old_lines[i]));
                out.push(format!("+ {}", new_lines[j]));
                i += 1;
                j += 1;
            }
        } else if i < old_lines.len() {
            out.push(format!("- {}", old_lines[i]));
            i += 1;
        } else {
            out.push(format!("+ {}", new_lines[j]));
            j += 1;
        }
    }
    out.join("\n")
}

pub fn diff_stats(old: &str, new: &str) -> (usize, usize) {
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut i = 0usize;
    let mut j = 0usize;
    while i < old_lines.len() || j < new_lines.len() {
        if i < old_lines.len() && j < new_lines.len() {
            if old_lines[i] == new_lines[j] {
                i += 1;
                j += 1;
            } else {
                removed += 1;
                added += 1;
                i += 1;
                j += 1;
            }
        } else if i < old_lines.len() {
            removed += 1;
            i += 1;
        } else {
            added += 1;
            j += 1;
        }
    }
    (added, removed)
}

fn is_allowed_rel_path(rel: &str) -> bool {
    let rel = rel.trim();
    if rel.is_empty() {
        return false;
    }
    if rel.starts_with('/') || rel.starts_with('\\') {
        return false;
    }
    if rel.contains("..") {
        return false;
    }
    if rel == "dbt_project.yml" || rel == "packages.yml" {
        return true;
    }
    let allowed_prefixes = [
        "models/",
        "seeds/",
        "macros/",
        "snapshots/",
        "analyses/",
        "tests/",
        "target/",
    ];
    allowed_prefixes.iter().any(|p| rel.starts_with(p))
}

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let out = hasher.finalize();
    hex::encode(out)
}

async fn postprocess_content(
    ctx: &AgentCtx,
    datasets: Option<&std::sync::Arc<dyn DatasetCatalogProvider>>,
    rel: &str,
    content: &str,
) -> Result<String, String> {
    if rel == "packages.yml" {
        return postprocess_packages_yml(content);
    }
    if rel == "models/schema.yml" {
        return postprocess_schema_yml(ctx, datasets, content).await;
    }
    if rel.starts_with("models/") && rel.ends_with(".sql") {
        return postprocess_model_sql(ctx, rel, content);
    }
    Ok(content.to_string())
}

async fn postprocess_schema_yml(
    ctx: &AgentCtx,
    datasets: Option<&std::sync::Arc<dyn DatasetCatalogProvider>>,
    content: &str,
) -> Result<String, String> {
    let Some(datasets) = datasets else {
        return Err("dataset provider missing for schema.yml postprocess".to_string());
    };
    let cfg = crate::config::resolved_config_from_ctx(ctx)
        .ok_or_else(|| "resolved_config missing for schema.yml postprocess".to_string())?;
    let want_catalog = cfg.providers.athena.target_catalog.clone();
    let want_schema = cfg.providers.athena.source_schema.clone();
    let mut dss = datasets.list_datasets().await.map_err(|e| format!("list_datasets: {}", e))?;
    dss.retain(|ds| ds.catalog == want_catalog && ds.database == want_schema);
    dss.sort_by(|a, b| a.table.cmp(&b.table));
    let sources_val = sources_value_from_dataset_ids(&dss);

    let mut root = if content.trim().is_empty() {
        YamlMapping::new()
    } else {
        let v: YamlValue = serde_yaml::from_str(content).map_err(|e| format!("schema.yml parse error: {}", e))?;
        match v {
            YamlValue::Mapping(m) => m,
            _ => return Err("models/schema.yml must be a YAML mapping at top level".to_string()),
        }
    };

    if !root.contains_key(&YamlValue::String("version".to_string())) {
        root.insert(YamlValue::String("version".to_string()), YamlValue::Number(2.into()));
    }
    root.insert(YamlValue::String("sources".to_string()), sources_val);
    serde_yaml::to_string(&YamlValue::Mapping(root)).map_err(|e| e.to_string())
}

fn postprocess_packages_yml(content: &str) -> Result<String, String> {
    let mut root = if content.trim().is_empty() {
        YamlMapping::new()
    } else {
        let v: YamlValue = serde_yaml::from_str(content).map_err(|e| format!("packages.yml parse error: {}", e))?;
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
        let YamlValue::Mapping(m) = item else { continue };
        let key = package_entry_key(&m).unwrap_or_else(|| format!("unknown:{}", by_key.len()));
        let canonical = canonicalize_package_entry(&m);
        by_key.insert(key, YamlValue::Mapping(canonical));
    }

    let normalized: Vec<YamlValue> = by_key.into_values().collect();
    root.insert(YamlValue::String("packages".to_string()), YamlValue::Sequence(normalized));
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
        out.insert(YamlValue::String("package".to_string()), YamlValue::String(p));
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
        if ks == "package" || ks == "git" || ks == "local" || ks == "version" || ks == "revision" || ks == "subdir" {
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

fn sources_value_from_dataset_ids(dss: &[DatasetId]) -> YamlValue {
    let mut by_cat_db: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for ds in dss {
        by_cat_db.entry((ds.catalog.clone(), ds.database.clone())).or_default().push(ds.table.clone());
    }
    let mut sources_seq: Vec<YamlValue> = Vec::new();
    for ((cat, db), mut tables) in by_cat_db.into_iter() {
        tables.sort();
        tables.dedup();
        let mut src = YamlMapping::new();
        src.insert(YamlValue::String("name".to_string()), YamlValue::String(db.clone()));
        src.insert(YamlValue::String("database".to_string()), YamlValue::String(cat));
        src.insert(YamlValue::String("schema".to_string()), YamlValue::String(db));
        let mut tables_seq: Vec<YamlValue> = Vec::new();
        for t in tables.into_iter() {
            let mut tm = YamlMapping::new();
            tm.insert(YamlValue::String("name".to_string()), YamlValue::String(t));
            tables_seq.push(YamlValue::Mapping(tm));
        }
        src.insert(YamlValue::String("tables".to_string()), YamlValue::Sequence(tables_seq));
        sources_seq.push(YamlValue::Mapping(src));
    }
    YamlValue::Sequence(sources_seq)
}

fn postprocess_model_sql(ctx: &AgentCtx, rel: &str, content: &str) -> Result<String, String> {
    let cfg = crate::config::resolved_config_from_ctx(ctx)
        .ok_or_else(|| "resolved_config missing for model SQL postprocess".to_string())?;
    let suffix = tier_suffix_for_path(rel, &cfg).ok_or_else(|| "unable to infer tier suffix for model path".to_string())?;
    let alias = model_alias_from_rel(rel).ok_or_else(|| "unable to infer model alias from path".to_string())?;
    Ok(rewrite_config_header(content, &suffix, &alias))
}

fn tier_suffix_for_path(rel: &str, cfg: &crate::config::ReactResolvedConfig) -> Option<String> {
    if rel.starts_with("models/staging/") {
        return Some(cfg.providers.dbt.naming.silver_suffix.clone());
    }
    if rel.starts_with("models/core/") || rel.starts_with("models/marts/") {
        return Some(cfg.providers.dbt.naming.gold_suffix.clone());
    }
    if rel.starts_with("models/") {
        return Some(cfg.providers.dbt.naming.gold_suffix.clone());
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

fn rewrite_config_header(content: &str, schema_suffix: &str, alias: &str) -> String {
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
        return format!("{{{{ config(schema=\"{}\", alias=\"{}\") }}}}\n", schema_suffix, alias);
    }
    format!(
        "{{{{ config(schema=\"{}\", alias=\"{}\") }}}}\n\n{}",
        schema_suffix, alias, body
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::{ChatMessage, LargeLanguageModel};
    use react_core::scope::RequestScope;
    use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};
    use std::sync::Arc;

    #[derive(Default)]
    struct DummyLlm;

    impl LargeLanguageModel for DummyLlm {
        fn chat(&self, _messages: &[ChatMessage]) -> Result<String, String> {
            Err("not used".to_string())
        }
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
    }

    #[derive(Clone)]
    struct MockDatasets {
        items: Vec<DatasetId>,
    }

    #[async_trait]
    impl DatasetCatalogProvider for MockDatasets {
        async fn list_datasets(&self) -> Result<Vec<DatasetId>, String> {
            Ok(self.items.clone())
        }
        async fn get_dataset_schema(&self, _dataset: &DatasetId) -> Result<Vec<(String, String)>, String> {
            Ok(vec![])
        }
        async fn get_dataset_stats(
            &self,
            _dataset: &DatasetId,
            _max_fields: usize,
        ) -> Result<(react_core::discover::stats::DatasetFieldStats, react_core::providers::catalog::types::DatasetStats), String> {
            Err("not implemented".to_string())
        }
    }

    fn minimal_cfg() -> Arc<crate::config::ReactResolvedConfig> {
        Arc::new(crate::config::ReactResolvedConfig {
            server: crate::config::ServerResolved { port: 1 },
            storage: crate::config::StorageResolved { bucket: "b".to_string() },
            scope: RequestScope { tenant: "t".to_string(), workspace: "w".to_string(), project_id: "p".to_string() },
            llm: crate::config::LlmResolved::default(),
            providers: crate::config::ProvidersResolved {
                athena: crate::config::AthenaResolved {
                    enabled: true,
                    workgroup: "wg".to_string(),
                    region: "eu-west-1".to_string(),
                    result_s3: "s3://x/".to_string(),
                    target_catalog: "AwsDataCatalog".to_string(),
                    source_schema: "test_raw".to_string(),
                    discovery_cache_ttl_secs: 120,
                },
                catalog: crate::config::CatalogResolved { enabled: false, refresh_secs: 60, max_concurrency: 8 },
                dbt: crate::config::DbtResolved {
                    enabled: true,
                    profiles_dir: None,
                    target: "athena".to_string(),
                    naming: crate::config::DbtNamingResolved {
                        target_schema: "test".to_string(),
                        silver_suffix: "silver".to_string(),
                        gold_suffix: "warehouse".to_string(),
                    },
                    runner: "host".to_string(),
                    docker_image: None,
                    docker_platform: None,
                    docker_network: None,
                    docker_mount_aws_dir: false,
                },
                vector: crate::config::VectorResolved { enabled: false },
            },
        })
    }

    fn make_ctx(storage: Arc<dyn StorageAdapter>) -> AgentCtx {
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope { tenant: "t".to_string(), workspace: "w".to_string(), project_id: "p".to_string() };
        AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 1,
            thread_id: None,
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm: Arc::new(DummyLlm::default()),
            storage,
            scope: scope.clone(),
            keyspace,
            query: None,
            dbt: None,
            vector: None,
            thread_store: None,
            runtime: Some(minimal_cfg() as Arc<dyn std::any::Any + Send + Sync>),
        }
    }

    #[tokio::test]
    async fn apply_patch_creates_file_and_injects_config() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage);
        let patch_text = create_patch_text("", "select 1");
        let outcome = apply_patch(&ctx, None, "models/staging/stg_orders.sql", &patch_text, None, true)
            .await
            .expect("apply patch");
        assert!(outcome.content.contains("config(schema=\"silver\""));
        assert!(outcome.content.contains("alias=\"stg_orders\""));
    }

    #[tokio::test]
    async fn apply_patch_schema_yml_rebuilds_sources_and_preserves_models() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage);
        let datasets: Arc<dyn DatasetCatalogProvider> = Arc::new(MockDatasets {
            items: vec![
                DatasetId { catalog: "AwsDataCatalog".to_string(), database: "test_raw".to_string(), table: "raw_customers".to_string() },
                DatasetId { catalog: "AwsDataCatalog".to_string(), database: "test_raw".to_string(), table: "raw_orders".to_string() },
            ],
        });
        let existing = "version: 2\nmodels:\n  - name: stg_raw_customers\n";
        let patch_text = create_patch_text("", existing);
        let outcome = apply_patch(&ctx, Some(&datasets), "models/schema.yml", &patch_text, None, true)
            .await
            .expect("apply patch");
        let v: YamlValue = serde_yaml::from_str(&outcome.content).expect("valid yaml");
        let map = v.as_mapping().expect("mapping root");
        assert!(map.contains_key(&YamlValue::String("models".to_string())));
        assert!(map.contains_key(&YamlValue::String("sources".to_string())));
    }

    #[tokio::test]
    async fn apply_patch_packages_yml_normalizes_entries() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage);
        let raw = r#"
packages:
  - package: calogica/dbt_expectations
    version: [">=0.8.0", "<1.0.0"]
  - package: dbt-labs/dbt_utils
    version: [">=1.0.0", "<2.0.0"]
  - package: dbt-labs/dbt_utils
    version: [">=1.0.0", "<2.0.0"]
"#;
        let patch_text = create_patch_text("", raw);
        let outcome = apply_patch(&ctx, None, "packages.yml", &patch_text, None, true)
            .await
            .expect("apply patch");
        let v: YamlValue = serde_yaml::from_str(&outcome.content).expect("valid yaml");
        let map = v.as_mapping().expect("mapping root");
        let packages = map
            .get(&YamlValue::String("packages".to_string()))
            .and_then(|v| v.as_sequence())
            .expect("packages list");
        assert_eq!(packages.len(), 2);
        let first = packages[0].as_mapping().expect("mapping");
        let first_pkg = first
            .get(&YamlValue::String("package".to_string()))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(first_pkg, "calogica/dbt_expectations");
    }

    #[tokio::test]
    async fn apply_patch_rejects_base_sha_mismatch() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage);
        let patch_text = create_patch_text("", "select 1");
        let err = apply_patch(&ctx, None, "models/staging/stg_orders.sql", &patch_text, Some("bad"), true)
            .await
            .unwrap_err();
        assert!(err.contains("base_sha256 mismatch"));
    }
}
