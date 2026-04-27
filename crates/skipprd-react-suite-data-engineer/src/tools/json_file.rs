use async_trait::async_trait;
use serde_json::Value;

use react_core::agent::AgentCtx;
use react_core::storage::retry_get_bytes;
use react_core::tools::Tool;

pub struct JsonFileTool;

#[async_trait]
impl Tool for JsonFileTool {
    fn name(&self) -> &'static str {
        "json_file"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let op = args
            .get("op")
            .and_then(|x| x.as_str())
            .unwrap_or("get_item");
        match op {
            "get_item" => get_item(args, ctx).await,
            "query" => query(args, ctx).await,
            _ => Err("unsupported op; use 'get_item' or 'query'".to_string()),
        }
    }
}

async fn get_item(args: Value, ctx: &AgentCtx) -> Result<Value, String> {
    let path = args
        .get("path")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "path required".to_string())?;
    let pointer = args.get("pointer").and_then(|x| x.as_str());

    let rel = crate::project_fs::normalize_rel_path(path)?;
    let key = crate::project_fs::join_storage_key(ctx, &rel);
    let bytes = match retry_get_bytes(ctx.storage().as_ref(), &key).await {
        Ok(b) => b,
        Err(e) => {
            return Ok(
                serde_json::json!({"ok": false, "path": rel, "key": key, "error": format!("not found or failed to fetch: {}", e)}),
            )
        }
    };
    let parsed: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return Ok(
                serde_json::json!({"ok": false, "path": rel, "key": key, "error": format!("failed to parse json: {}", e)}),
            )
        }
    };

    if let Some(ptr) = pointer {
        let ptr = ptr.trim();
        if ptr.is_empty() {
            return Ok(serde_json::json!({"ok": true, "path": rel, "key": key, "json": parsed}));
        }
        if let Some(sub) = parsed.pointer(ptr) {
            return Ok(
                serde_json::json!({"ok": true, "path": rel, "key": key, "pointer": ptr, "json": sub}),
            );
        }
        return Ok(
            serde_json::json!({"ok": false, "path": rel, "key": key, "pointer": ptr, "error": "pointer not found"}),
        );
    }

    Ok(serde_json::json!({"ok": true, "path": rel, "key": key, "json": parsed}))
}

async fn query(args: Value, ctx: &AgentCtx) -> Result<Value, String> {
    let path = args
        .get("path")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "path required".to_string())?;
    let pointer = args
        .get("pointer")
        .and_then(|x| x.as_str())
        .unwrap_or("/nodes");
    let unique_id = args.get("unique_id").and_then(|x| x.as_str());
    let name = args.get("name").and_then(|x| x.as_str());
    let resource_type = args.get("resource_type").and_then(|x| x.as_str());
    let limit = args
        .get("limit")
        .and_then(|x| x.as_u64())
        .unwrap_or(20)
        .min(200) as usize;

    let rel = crate::project_fs::normalize_rel_path(path)?;
    let key = crate::project_fs::join_storage_key(ctx, &rel);
    let bytes = match retry_get_bytes(ctx.storage().as_ref(), &key).await {
        Ok(b) => b,
        Err(e) => {
            return Ok(
                serde_json::json!({"ok": false, "path": rel, "key": key, "error": format!("not found or failed to fetch: {}", e)}),
            )
        }
    };
    let parsed: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return Ok(
                serde_json::json!({"ok": false, "path": rel, "key": key, "error": format!("failed to parse json: {}", e)}),
            )
        }
    };

    let Some(scope) = parsed.pointer(pointer) else {
        return Ok(
            serde_json::json!({"ok": false, "path": rel, "key": key, "pointer": pointer, "error": "pointer not found"}),
        );
    };

    let mut items: Vec<Value> = Vec::new();
    match scope {
        Value::Object(map) => {
            for (k, v) in map {
                let node_name = v.get("name").and_then(|x| x.as_str()).unwrap_or("");
                let node_rt = v
                    .get("resource_type")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                if let Some(want) = unique_id {
                    if k != want {
                        continue;
                    }
                }
                if let Some(want) = name {
                    if node_name != want {
                        continue;
                    }
                }
                if let Some(want) = resource_type {
                    if node_rt != want {
                        continue;
                    }
                }
                let mut slim = serde_json::Map::new();
                slim.insert("unique_id".to_string(), serde_json::json!(k));
                for field in [
                    "resource_type",
                    "name",
                    "original_file_path",
                    "path",
                    "package_name",
                    "database",
                    "schema",
                    "alias",
                ] {
                    if let Some(val) = v.get(field) {
                        slim.insert(field.to_string(), val.clone());
                    }
                }
                if let Some(dep) = v.get("depends_on") {
                    slim.insert("depends_on".to_string(), dep.clone());
                }
                if slim.len() == 1 {
                    slim.insert("value".to_string(), v.clone());
                }
                items.push(Value::Object(slim));
                if items.len() >= limit {
                    break;
                }
            }
        }
        Value::Array(arr) => {
            for v in arr {
                let node_uid = v.get("unique_id").and_then(|x| x.as_str()).unwrap_or("");
                let node_name = v.get("name").and_then(|x| x.as_str()).unwrap_or("");
                let node_rt = v
                    .get("resource_type")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                if let Some(want) = unique_id {
                    if node_uid != want {
                        continue;
                    }
                }
                if let Some(want) = name {
                    if node_name != want {
                        continue;
                    }
                }
                if let Some(want) = resource_type {
                    if node_rt != want {
                        continue;
                    }
                }
                items.push(v.clone());
                if items.len() >= limit {
                    break;
                }
            }
        }
        _ => {
            return Ok(
                serde_json::json!({"ok": false, "path": rel, "key": key, "pointer": pointer, "error": "query pointer must resolve to an object or array"}),
            );
        }
    }

    Ok(serde_json::json!({
        "ok": true,
        "path": rel,
        "key": key,
        "pointer": pointer,
        "items": items
    }))
}
