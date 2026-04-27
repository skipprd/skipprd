use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::providers::QueryProvider;
use react_core::agent::AgentCtx;
use react_core::tools::Tool;

pub struct SqlRunTool {
    pub query: Arc<dyn QueryProvider>,
}

#[async_trait]
impl Tool for SqlRunTool {
    fn name(&self) -> &'static str {
        "run_sql"
    }
    async fn call(&self, args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
        let sql = args.get("sql").and_then(|x| x.as_str()).unwrap_or("");
        let mut forced = sql.trim().to_string();
        // Append a LIMIT to plain SELECT/CTE queries that don't specify one, to avoid huge outputs.
        let up = forced.to_uppercase();
        let starts_with_select = up.starts_with("SELECT ");
        let starts_with_with = up.starts_with("WITH ");
        let has_limit = forced
            .split_whitespace()
            .any(|w| w.eq_ignore_ascii_case("LIMIT"));
        if (starts_with_select || starts_with_with) && !has_limit {
            forced.push_str(" LIMIT 50");
        }
        let normalized_sql = forced
            .split_whitespace()
            .collect::<Vec<&str>>()
            .join(" ")
            .to_ascii_lowercase();
        match crate::transient_retry::retry_transient_default("sql_run_query", || async {
            self.query.query(&forced).await
        })
        .await
        {
            Ok(qr) => {
                let row_count = qr.rows.len();
                let header_count = qr.header.len();
                let first_row_fingerprint = qr
                    .rows
                    .first()
                    .and_then(|row| serde_json::to_string(row).ok());
                Ok(serde_json::json!({
                    "ok": true,
                    "header": qr.header,
                    "rows": qr.rows,
                    "meta": qr.meta,
                    "probe": {
                        "normalized_sql": normalized_sql,
                        "row_count": row_count,
                        "header_count": header_count,
                        "first_row_fingerprint": first_row_fingerprint
                    }
                }))
            }
            Err(e) => Ok(serde_json::json!({"ok": false, "error": e})),
        }
    }
}
