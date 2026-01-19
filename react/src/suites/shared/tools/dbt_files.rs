use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;
use crate::agent::AgentCtx;
use crate::tools::Tool;

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
    // If the prefix points at a file, just return that file if it exists.
    if !rel_prefix.ends_with('/') && !rel_prefix.ends_with(".sql") && !rel_prefix.ends_with(".yml") && !rel_prefix.ends_with(".yaml") && !rel_prefix.ends_with(".json") {
        // Allow listing any directory-ish prefix; if user supplies "models" normalize to "models".
    }
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
            // Optional output limiting for very large files (e.g. target/manifest.json).
            // Prefer using get_json/manifest_find for structured access.
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
    // Specialized helper to avoid dumping full target/manifest.json into the model context.
    // Usage:
    // - dbt_files op=manifest_find name:"not_null_stg_orders_placed_at" (resource_type optional)
    // - dbt_files op=manifest_find unique_id:"test.test_project.not_null_..." (exact)
    // - optional path (defaults to target/manifest.json)
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
        let resource_type = node.get("resource_type").and_then(|x| x.as_str()).unwrap_or("");
        let name = node.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let relation_name = node.get("relation_name").and_then(|x| x.as_str());
        let database = node.get("database").and_then(|x| x.as_str());
        let schema = node.get("schema").and_then(|x| x.as_str());
        let alias = node.get("alias").and_then(|x| x.as_str());
        let original_file_path = node.get("original_file_path").and_then(|x| x.as_str());
        let depends_on = node
            .get("depends_on")
            .and_then(|d| d.get("nodes"))
            .and_then(|n| n.as_array())
            .cloned()
            .unwrap_or_default();

        out.push(serde_json::json!({
            "unique_id": uid,
            "resource_type": resource_type,
            "name": name,
            "relation_name": relation_name,
            "database": database,
            "schema": schema,
            "alias": alias,
            "original_file_path": original_file_path,
            "depends_on_nodes": depends_on,
        }));
        if out.len() >= limit {
            break;
        }
    }

    Ok(serde_json::json!({
        "ok": true,
        "path": rel,
        "key": key,
        "count": out.len(),
        "matches": out
    }))
}

async fn put_file(args: Value, ctx: &AgentCtx) -> Result<Value, String> {
    let path = args.get("path").and_then(|x| x.as_str()).ok_or_else(|| "path required".to_string())?;
    let rel = normalize_rel_path(path)?;
    let content = args.get("content").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if content.trim().is_empty() {
        return Err("content required".to_string());
    }
    let preview_diff = args.get("preview_diff").and_then(|x| x.as_bool()).unwrap_or(false);
    let key = join_storage_key(ctx, &rel);

    let existing: Option<String> = match ctx.storage.get_bytes(&key).await {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).to_string()),
        Err(_) => None,
    };

    if preview_diff {
        let diff = compute_unified_diff(existing.as_deref().unwrap_or(""), &content);
        return Ok(serde_json::json!({
            "ok": true,
            "preview_diff": true,
            "exists": existing.is_some(),
            "path": rel,
            "key": key,
            "diff": diff
        }));
    }

    let content_type = content_type_for_rel_path(&rel);
    ctx.storage.put_bytes(&key, content.as_bytes(), content_type).await?;
    let (lines_added, lines_removed) = line_delta(existing.as_deref().unwrap_or(""), &content);
    Ok(serde_json::json!({
        "ok": true,
        "path": rel,
        "key": key,
        "status": if existing.is_some() { "modified" } else { "created" },
        "lines_added": lines_added,
        "lines_removed": lines_removed
    }))
}

fn content_type_for_rel_path(rel: &str) -> &'static str {
    let p = Path::new(rel);
    match p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str() {
        "sql" => "text/sql",
        "yml" | "yaml" => "text/yaml",
        "json" => "application/json",
        "md" | "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

fn line_delta(old: &str, new: &str) -> (usize, usize) {
    // Very rough accounting; good enough for telemetry/UI.
    let old_lines = old.lines().count();
    let new_lines = new.lines().count();
    if new_lines >= old_lines {
        (new_lines - old_lines, 0)
    } else {
        (0, old_lines - new_lines)
    }
}

