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
			let mut forced = normalize_sql_identifiers(s);
			// Ensure referenced datasets are registered before execution (best-effort)
			{
				let ctx = if let Some(tid) = _ctx.thread_id.as_ref() {
					crate::ws::agent_runner::get_or_create_thread_ctx(tid)
				} else {
					self.ctx.clone()
				};
				let pairs = extract_sql_datasets(&forced);
				if !pairs.is_empty() {
					crate::ws::agent_runner::pre_register_selected_namespaces(&ctx, &pairs).await;
				}
			}
        // Append a LIMIT to plain SELECT/CTE queries that don't specify one, to avoid huge outputs
        let up = forced.to_uppercase();
        let starts_with_select = up.starts_with("SELECT ");
        let starts_with_with = up.starts_with("WITH ");
        if (starts_with_select || starts_with_with) && !forced.to_lowercase().contains(" limit ") {
            forced.push_str(" LIMIT 50");
        }
			// Always use the existing thread-scoped context if available; else fall back to injected ctx
			let ctx = if let Some(tid) = _ctx.thread_id.as_ref() {
				crate::ws::agent_runner::get_or_create_thread_ctx(tid)
			} else {
				self.ctx.clone()
			};
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

// Extract candidate <pipeline>.<namespace> references from SQL near FROM/JOIN clauses.
fn extract_sql_datasets(sql: &str) -> Vec<(String, String)> {
	let delimiters: &[char] = &[' ', '\n', '\t', ',', ';', '(', ')'];
	let mut pairs: Vec<(String, String)> = Vec::new();
	let mut tokens: Vec<String> = Vec::new();
	// Tokenize using simple delimiter split while keeping order
	let mut i = 0usize;
	while i < sql.len() {
		let rest = &sql[i..];
		let end = rest.find(delimiters).unwrap_or(rest.len());
		let tok = &rest[..end];
		if !tok.is_empty() {
			tokens.push(tok.to_string());
		}
		i += end;
		if end < rest.len() {
			// push the delimiter as a separate "token" only if it's a newline to preserve statement edges (optional)
			i += 1;
		}
	}
	// Scan tokens and capture table identifiers after FROM/JOIN
	let mut idx = 0usize;
	while idx < tokens.len() {
		let t = &tokens[idx];
		let tl = t.to_ascii_lowercase();
		let is_from_or_join = tl == "from" || tl.ends_with("join"); // covers JOIN, LEFT JOIN, INNER JOIN tokenization variants
		if is_from_or_join {
			// next non-empty token is a potential table identifier
			if let Some(next) = tokens.get(idx + 1) {
				let ident_norm = normalize_identifier(next);
				if let Some((p, n)) = split_pipeline_namespace(&ident_norm) {
					if !pairs.iter().any(|(pp, nn)| pp == &p && nn == &n) {
						pairs.push((p, n));
					}
				}
			}
		}
		idx += 1;
	}
	pairs
}

// Normalize a single identifier token by removing leading datafusion. and collapsing duplicate ns.
fn normalize_identifier(tok: &str) -> String {
	let mut t = tok.trim_matches('"').trim_matches('`').to_string();
	if let Some(rest) = t.strip_prefix("datafusion.") {
		t = rest.to_string();
	}
	let parts: Vec<&str> = t.split('.').collect();
	if parts.len() == 3 && parts[1].eq_ignore_ascii_case(parts[2]) {
		return format!("{}.{}", parts[0], parts[1]);
	}
	t
}

fn split_pipeline_namespace(ident: &str) -> Option<(String, String)> {
	let parts: Vec<&str> = ident.split('.').collect();
	match parts.len() {
		2 => {
			let p = parts[0].to_string();
			let n = parts[1].to_string();
			if !p.is_empty() && !n.is_empty() { Some((p, n)) } else { None }
		}
		3 => {
			// Treat as catalog.schema.table → drop catalog, use schema.table
			let p = parts[1].to_string();
			let n = parts[2].to_string();
			if !p.is_empty() && !n.is_empty() { Some((p, n)) } else { None }
		}
		_ => None,
	}
}


