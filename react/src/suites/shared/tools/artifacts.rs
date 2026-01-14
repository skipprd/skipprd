use async_trait::async_trait;
use serde_json::Value;
use crate::agent::AgentCtx;
use crate::tools::Tool;

pub struct ArtifactsTool;

#[async_trait]
impl Tool for ArtifactsTool {
	fn name(&self) -> &'static str { "artifacts" }
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
	let dataset_id_filter = args.get("dataset_id").and_then(|x| x.as_str()).map(|s| s.to_string());
	let limit = args.get("limit").and_then(|x| x.as_u64()).unwrap_or(20) as usize;
	let mut items: Vec<Value> = Vec::new();
	// ReAct is engine-agnostic: enumerate artifacts within the current project_id only.
	let base = ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string();
		let kinds: &[(&str, &str)] = &[
			("metric", "metrics"),
			("model", "models"),
		];
		for (kind_name, dir_name) in kinds {
			if let Some(t) = ty { if t != *kind_name { continue; } }
			let prefix = format!("{}/{}/", base, dir_name);
			let keys = ctx.storage.list_prefix(&prefix).await.unwrap_or_default();
			for k in keys {
				if k.contains("/_versions/") { continue; }
				let rest = k.strip_prefix(&prefix).unwrap_or(&k);
				let parts: Vec<&str> = rest.split('/').collect();
				if parts.len() != 2 { continue; }
				let ds_dir = parts[0].to_string();
				let dataset_id = percent_decode(&ds_dir);
				if let Some(filt) = dataset_id_filter.as_ref() {
					if &dataset_id != filt { continue; }
				}
				let name_ext = parts[1];
				let name = name_ext.trim_end_matches(".sql").trim_end_matches(".yaml").to_string();
				items.push(serde_json::json!({
					"dataset_id": dataset_id,
					"kind": *kind_name,
					"name": name,
					"key": k
				}));
			}
		}
	items.sort_by(|a, b| a.get("key").and_then(|x| x.as_str()).cmp(&b.get("key").and_then(|x| x.as_str())));
	let listed: Vec<Value> = items.into_iter().take(limit).collect();
	Ok(serde_json::json!({"ok": true, "items": listed}))
}

fn percent_decode(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	let bytes = s.as_bytes();
	let mut i = 0usize;
	while i < bytes.len() {
		if bytes[i] == b'%' && i + 2 < bytes.len() {
			let h1 = bytes[i + 1];
			let h2 = bytes[i + 2];
			let hex = |b: u8| -> Option<u8> {
				match b {
					b'0'..=b'9' => Some(b - b'0'),
					b'a'..=b'f' => Some(b - b'a' + 10),
					b'A'..=b'F' => Some(b - b'A' + 10),
					_ => None,
				}
			};
			if let (Some(a), Some(b)) = (hex(h1), hex(h2)) {
				out.push((a * 16 + b) as char);
				i += 3;
				continue;
			}
		}
		out.push(bytes[i] as char);
		i += 1;
	}
	out
}

async fn get_artifact(args: Value, ctx: &AgentCtx) -> Result<Value, String> {
	let kind = args.get("type").and_then(|x| x.as_str()).unwrap_or("");
	if kind != "model" && kind != "metric" {
		return Err("type must be 'model' or 'metric'".to_string());
	}
	let dataset_id = args.get("dataset_id").and_then(|x| x.as_str()).map(|s| s.to_string());
	let name = args.get("name").and_then(|x| x.as_str()).map(|s| s.to_string()).ok_or_else(|| "name required".to_string())?;
	let ds = match dataset_id {
		Some(ds) => ds,
		None => return Err("dataset_id required".to_string()),
	};
	let base = ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string();
	let dir = encode_key_component(&ds);
	let key = match kind {
		"model" => format!("{}/models/{}/{}.sql", base, dir, name),
		_ => format!("{}/metrics/{}/{}.yaml", base, dir, name),
	};
	match ctx.storage.get_bytes(&key).await {
		Ok(bytes) => {
			let text = String::from_utf8_lossy(&bytes).to_string();
			Ok(serde_json::json!({"ok": true, "key": key, "content": text}))
		}
		Err(e) => Err(format!("not found or failed to fetch: {}", e)),
	}
}

fn encode_key_component(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for b in s.as_bytes() {
		let c = *b as char;
		let safe = c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.';
		if safe {
			out.push(c);
		} else {
			out.push_str(&format!("%{:02X}", b));
		}
	}
	out
}


