use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;

use react_core::agent::AgentCtx;
use react_core::tools::Tool;

pub struct DbtFilesTool;

#[async_trait]
impl Tool for DbtFilesTool {
    fn name(&self) -> &'static str {
        "dbt_files"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let op = args.get("op").and_then(|x| x.as_str()).unwrap_or("get");
        match op {
            "list" => list_files(args, ctx).await,
            "get" => get_file(args, ctx).await,
            "get_json" => get_json(args, ctx).await,
            "manifest_find" => manifest_find(args, ctx).await,
            "put" => put_file(args, ctx).await,
            _ => Err("unsupported op; use 'list', 'get', 'get_json', 'manifest_find', or 'put'".to_string()),
        }
    }
}

fn compute_unified_diff(old: &str, new: &str) -> String {
    // Simple line-wise diff (kept intentionally small and dependency-free)
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

fn is_allowed_rel_path(rel: &str) -> bool {
    let rel = rel.trim();
    if rel.is_empty() {
        return false;
    }
    // No absolute paths
    if rel.starts_with('/') || rel.starts_with('\\') {
        return false;
    }
    // No parent traversal
    if rel.contains("..") {
        return false;
    }
    // Keep within a known subset of dbt project files/dirs.
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

fn normalize_rel_path(rel: &str) -> Result<String, String> {
    let rel = rel.trim().trim_start_matches("./").to_string();
    if !is_allowed_rel_path(&rel) {
        return Err("path not allowed; only dbt project files under models/, seeds/, macros/, snapshots/, analyses/, tests/, target/ (or dbt_project.yml / packages.yml) are permitted".to_string());
    }
    // Normalize separators to '/' for storage keys.
    Ok(rel.replace('\\', "/"))
}

fn join_storage_key(ctx: &AgentCtx, rel: &str) -> String {
    let base = ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string();
    format!("{}/{}", base, rel)
}

async fn list_files(args: Value, ctx: &AgentCtx) -> Result<Value, String> {
    let prefix = args.get("prefix").and_then(|x| x.as_str()).unwrap_or("").trim();
    let limit = args.get("limit").and_then(|x| x.as_u64()).unwrap_or(200).min(2000) as usize;
    let rel_prefix = if prefix.is_empty() { "models/".to_string() } else { normalize_rel_path(prefix)? };
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

async fn get_file(args: Value, ctx: &AgentCtx) -> Result<Value, String> {
    let path = args.get("path").and_then(|x| x.as_str()).ok_or_else(|| "path required".to_string())?;
    let rel = normalize_rel_path(path)?;
    let key = join_storage_key(ctx, &rel);
    match ctx.storage.get_bytes(&key).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes).to_string();
            let max_chars = args.get("max_chars").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
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

async fn get_json(args: Value, ctx: &AgentCtx) -> Result<Value, String> {
    let path = args.get("path").and_then(|x| x.as_str()).ok_or_else(|| "path required".to_string())?;
    let rel = normalize_rel_path(path)?;
    let key = join_storage_key(ctx, &rel);
    let bytes = match ctx.storage.get_bytes(&key).await {
        Ok(b) => b,
        Err(e) => return Ok(serde_json::json!({"ok": false, "path": rel, "key": key, "error": format!("not found or failed to fetch: {}", e)})),
    };
    let v: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return Ok(serde_json::json!({"ok": false, "path": rel, "key": key, "error": format!("failed to parse json: {}", e)}));
        }
    };
    if let Some(ptr) = args.get("pointer").and_then(|x| x.as_str()) {
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

async fn manifest_find(args: Value, ctx: &AgentCtx) -> Result<Value, String> {
    let path = args
        .get("path")
        .and_then(|x| x.as_str())
        .unwrap_or("target/manifest.json");
    let rel = normalize_rel_path(path)?;
    let key = join_storage_key(ctx, &rel);
    let bytes = match ctx.storage.get_bytes(&key).await {
        Ok(b) => b,
        Err(e) => return Ok(serde_json::json!({"ok": false, "path": rel, "key": key, "error": format!("not found or failed to fetch: {}", e)})),
    };
    let v: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return Ok(serde_json::json!({"ok": false, "path": rel, "key": key, "error": format!("failed to parse json: {}", e)})),
    };
    let nodes = match v.get("nodes").and_then(|n| n.as_object()) {
        Some(n) => n,
        None => return Ok(serde_json::json!({"ok": false, "path": rel, "key": key, "error": "manifest missing nodes"})),
    };

    let want_unique_id = args.get("unique_id").and_then(|x| x.as_str()).map(|s| s.to_string());
    let want_name = args.get("name").and_then(|x| x.as_str()).map(|s| s.to_string());
    let want_resource_type = args.get("resource_type").and_then(|x| x.as_str()).map(|s| s.to_string());
    let limit = args.get("limit").and_then(|x| x.as_u64()).unwrap_or(20).min(200) as usize;

    let mut out: Vec<serde_json::Value> = Vec::new();
    for (uid, node) in nodes.iter() {
        if let Some(ref u) = want_unique_id {
            if uid != u {
                continue;
            }
        }
        if let Some(ref n) = want_name {
            let node_name = node.get("name").and_then(|x| x.as_str()).unwrap_or("");
            if node_name != n {
                continue;
            }
        }
        if let Some(ref rt) = want_resource_type {
            let node_rt = node.get("resource_type").and_then(|x| x.as_str()).unwrap_or("");
            if node_rt != rt {
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

async fn put_file(args: Value, ctx: &AgentCtx) -> Result<Value, String> {
    let path = args.get("path").and_then(|x| x.as_str()).ok_or_else(|| "path required".to_string())?;
    let rel = normalize_rel_path(path)?;
    let content = args.get("content").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let preview = args.get("preview_diff").and_then(|x| x.as_bool()).unwrap_or(false);

    let key = join_storage_key(ctx, &rel);
    let old = ctx.storage.get_bytes(&key).await.ok().map(|b| String::from_utf8_lossy(&b).to_string()).unwrap_or_default();

    // guard against empty writes for yaml/sql to reduce accidental nukes
    if (rel.ends_with(".yml") || rel.ends_with(".yaml") || rel.ends_with(".sql")) && content.trim().is_empty() {
        return Err("refusing to write empty content".to_string());
    }
    // simple file extension guard
    if let Some(ext) = Path::new(&rel).extension().and_then(|s| s.to_str()) {
        let ext = ext.to_ascii_lowercase();
        let allowed = ["yml","yaml","sql","json","md","txt","csv"];
        if !allowed.iter().any(|a| a == &ext) {
            return Err("file extension not allowed".to_string());
        }
    }
    ctx.storage.put_bytes(&key, content.as_bytes(), "text/plain").await?;

    let diff = if preview { Some(compute_unified_diff(&old, &content)) } else { None };
    Ok(serde_json::json!({"ok": true, "path": rel, "key": key, "preview_diff": diff}))
}

