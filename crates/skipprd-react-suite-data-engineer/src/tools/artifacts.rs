use async_trait::async_trait;
use serde_json::Value;

use react_core::agent::AgentCtx;
use react_core::storage::{retry_get_bytes, retry_list_prefix};
use react_core::tools::Tool;

pub struct ArtifactsTool;

#[async_trait]
impl Tool for ArtifactsTool {
    fn name(&self) -> &'static str {
        "artifacts"
    }
    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let op = args.get("op").and_then(|x| x.as_str()).unwrap_or("list");
        match op {
            "list" => list_artifacts(args, ctx).await,
            "get" => get_artifact(args, ctx).await,
            _ => Err("unsupported op; use 'list' or 'get'".to_string()),
        }
    }
}

async fn list_artifacts(args: Value, ctx: &AgentCtx) -> Result<Value, String> {
    let ty = args.get("type").and_then(|x| x.as_str()); // "model"|"metric"|None
    let tier_filter = args.get("tier").and_then(|x| x.as_str());
    let limit = args.get("limit").and_then(|x| x.as_u64()).unwrap_or(20) as usize;
    let mut items: Vec<Value> = Vec::new();
    let base = ctx
        .keyspace()
        .scoped_prefix(ctx.scope(), &["dbt"])
        .trim_end_matches('/')
        .to_string();
    let kinds: &[(&str, &str, &str)] =
        &[("metric", "metrics", ".yaml"), ("model", "models", ".sql")];
    for (kind_name, dir_name, ext) in kinds {
        if let Some(t) = ty {
            if t != *kind_name {
                continue;
            }
        }
        let prefix = format!("{}/{}/", base, dir_name);
        let keys = retry_list_prefix(ctx.storage().as_ref(), &prefix)
            .await
            .unwrap_or_default();
        for k in keys {
            if k.contains("/_versions/") {
                continue;
            }
            let rest = k.strip_prefix(&prefix).unwrap_or(&k);
            if rest.trim().is_empty() || rest.ends_with('/') || !rest.ends_with(ext) {
                continue;
            }
            let tier = rest.split('/').next().unwrap_or("").to_string();
            if let Some(filt) = tier_filter {
                if tier != filt {
                    continue;
                }
            }
            let rel_path = format!("{}/{}", dir_name, rest);
            let name = rest
                .rsplit('/')
                .next()
                .unwrap_or(rest)
                .trim_end_matches(ext)
                .to_string();
            items.push(serde_json::json!({
                "kind": *kind_name,
                "name": name,
                "tier": tier,
                "rel_path": rel_path,
                "key": k
            }));
        }
    }
    items.sort_by(|a, b| {
        a.get("key")
            .and_then(|x| x.as_str())
            .cmp(&b.get("key").and_then(|x| x.as_str()))
    });
    let listed: Vec<Value> = items.into_iter().take(limit).collect();
    Ok(serde_json::json!({"ok": true, "items": listed}))
}

async fn get_artifact(args: Value, ctx: &AgentCtx) -> Result<Value, String> {
    let rel_path = args
        .get("path")
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "path required".to_string())?;
    if rel_path.starts_with('/') || rel_path.contains("..") {
        return Err("path must be a safe relative dbt path".to_string());
    }
    let kind = if rel_path.starts_with("models/") && rel_path.ends_with(".sql") {
        "model"
    } else if rel_path.starts_with("metrics/") && rel_path.ends_with(".yaml") {
        "metric"
    } else {
        return Err("path must target models/*.sql or metrics/*.yaml".to_string());
    };
    let base = ctx
        .keyspace()
        .scoped_prefix(ctx.scope(), &["dbt"])
        .trim_end_matches('/')
        .to_string();
    let key = format!("{}/{}", base, rel_path);
    match retry_get_bytes(ctx.storage().as_ref(), &key).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes).to_string();
            Ok(
                serde_json::json!({"ok": true, "kind": kind, "path": rel_path, "key": key, "content": text}),
            )
        }
        Err(e) => Err(format!("not found or failed to fetch: {}", e)),
    }
}
