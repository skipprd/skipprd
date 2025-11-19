use async_trait::async_trait;
use serde_json::Value;
use crate::qa::agent::AgentCtx;
use super::Tool;

pub struct SqlStatsTool;

#[async_trait]
impl Tool for SqlStatsTool {
    fn name(&self) -> &'static str { "sql_stats" }
    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let table = args.get("table").and_then(|x| x.as_str()).unwrap_or("");
        let field = args.get("field").and_then(|x| x.as_str()).unwrap_or("");
		let ns = if !table.is_empty() {
			table.to_string()
		} else if let Some(c) = ctx.dataset_candidates.first() {
			format!("{}.{}", c.pipeline, c.namespace)
		} else {
			String::new()
		};
		if ns.is_empty() || field.is_empty() {
            return Ok(serde_json::json!({"ok": false, "error": "missing table/field"}));
        }
        // Read stats from Catalog (preferred) or fallback to separate stats JSON
		let (pipeline, namespace) = if ns.contains('.') {
			let parts: Vec<&str> = ns.splitn(2, '.').collect();
			(parts[0].to_string(), parts[1].to_string())
		} else {
			return Ok(serde_json::json!({"ok": false, "error": "table must be fully-qualified <pipeline>.<namespace> or dataset_candidates must be present"}));
		};
        // Try catalog
        let mut distinct: Option<u64> = None;
        let mut min_numeric: Option<f64> = None;
        let mut max_numeric: Option<f64> = None;
        let mut max_len: Option<u64> = None;
        let mut nulls: u64 = 0;
        if let Some(entry) = crate::sql::registry::find_entry(&pipeline, &namespace).await {
            if !entry.catalog_key.is_empty() {
                if let Ok(cat) = crate::helpers::s3::get_json(&entry.catalog_key).await {
                    if let Some(fields) = cat.get("fields").and_then(|x| x.as_array()) {
                        for f in fields {
                            if f.get("name").and_then(|x| x.as_str()) == Some(field) {
                                if let Some(st) = f.get("stats").and_then(|x| x.as_object()) {
                                    distinct = st.get("approx_distinct").and_then(|x| x.as_u64());
                                    min_numeric = st.get("min_numeric").and_then(|x| x.as_f64());
                                    max_numeric = st.get("max_numeric").and_then(|x| x.as_f64());
                                    max_len = st.get("max_len").and_then(|x| x.as_u64());
                                    nulls = st.get("nulls").and_then(|x| x.as_u64()).unwrap_or(0);
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }
        if distinct.is_none() && min_numeric.is_none() && max_numeric.is_none() && max_len.is_none() && nulls == 0 {
            return Ok(serde_json::json!({"ok": false, "error": "no stats in catalog"}));
        }
        Ok(serde_json::json!({"ok": true, "stats": {
            "distinct": distinct,
            "min": min_numeric,
            "max": max_numeric,
            "max_len": max_len,
            "nulls": nulls
        }}))
    }
}


