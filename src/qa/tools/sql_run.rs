use async_trait::async_trait;
use serde_json::Value;
use crate::qa::agent::AgentCtx;
use super::Tool;
use datafusion::prelude::SessionContext;

pub struct SqlRunTool {
    pub ctx: SessionContext,
}

#[async_trait]
impl Tool for SqlRunTool {
    fn name(&self) -> &'static str { "run_sql" }
    async fn call(&self, args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
        let sql = args.get("sql").and_then(|x| x.as_str()).unwrap_or("");
			let s = sql.trim();
        if !s.to_uppercase().starts_with("SELECT ") { return Ok(serde_json::json!({"ok": false, "error": "only SELECT allowed"})); }
			let mut forced = normalize_sql_identifiers(s);
        if !forced.to_lowercase().contains(" limit ") {
            forced.push_str(" LIMIT 50");
        }
			// Use centralized DF context with all namespaces registered
			let ctx = crate::sql::query::new_context_all_namespaces().await;
			match ctx.sql(&forced).await {
            Ok(df) => match df.collect().await {
                Ok(batches) => {
                    let mut out_rows: Vec<Vec<String>> = Vec::new();
                    let mut header: Vec<String> = Vec::new();
                    if let Some(first) = batches.first() {
                        header = first.schema().fields().iter().map(|f| f.name().to_string()).collect();
                    }
                    for b in batches {
                        for r in 0..b.num_rows() {
                            let mut row: Vec<String> = Vec::new();
                            for c in 0..b.num_columns() {
                                row.push(crate::sql::tui::value_to_string(b.column(c).as_ref(), r));
                            }
                            out_rows.push(row);
                        }
                    }
                    Ok(serde_json::json!({"ok": true, "header": header, "rows": out_rows}))
                }
                Err(e) => Ok(serde_json::json!({"ok": false, "error": e.to_string()})),
            },
            Err(e) => Ok(serde_json::json!({"ok": false, "error": e.to_string()})),
        }
    }
}

// Normalize SQL identifiers:
// - Strip optional leading 'datafusion.' catalog from 3-part names
// - Collapse accidental duplicate namespace: pipeline.namespace.namespace -> pipeline.namespace
fn normalize_sql_identifiers(sql: &str) -> String {
	let delimiters: &[char] = &[' ', '\n', '\t', ',', ';', '(', ')'];
	let mut out = String::with_capacity(sql.len());
	let mut i = 0;
	while i < sql.len() {
		let rest = &sql[i..];
		let end = rest.find(delimiters).unwrap_or(rest.len());
		let tok = &rest[..end];
		let mut replaced = None::<String>;
		// Handle dot-separated identifiers
		if tok.contains('.') {
			let parts: Vec<&str> = tok.split('.').collect();
			if parts.len() >= 2 {
				// Case 1: strip leading 'datafusion'
				let mut start_idx = 0;
				if parts[0].eq_ignore_ascii_case("datafusion") && parts.len() >= 3 {
					start_idx = 1;
				}
				let slice = &parts[start_idx..];
				// Case 2: collapse duplicate trailing namespace pipeline.ns.ns
				let collapsed = if slice.len() == 3 && slice[1].eq_ignore_ascii_case(slice[2]) {
					format!("{}.{}", slice[0], slice[1])
				} else {
					slice.join(".")
				};
				replaced = Some(collapsed);
			}
		}
		if let Some(rep) = replaced {
			out.push_str(&rep);
		} else {
			out.push_str(tok);
		}
		i += end;
		if end < rest.len() {
			out.push(rest.as_bytes()[end] as char);
			i += 1;
		}
	}
	out
}


