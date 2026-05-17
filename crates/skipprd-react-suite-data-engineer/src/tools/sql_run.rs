use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::providers::QueryProvider;
use crate::sql_prepare::prepare_read_only_sql;
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
        let prepared = match prepare_read_only_sql(sql, 50) {
            Ok(prepared) => prepared,
            Err(e) => return Ok(serde_json::json!({"ok": false, "error": e})),
        };
        match crate::transient_retry::retry_transient_default("sql_run_query", || async {
            self.query.query(&prepared.sql).await
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
                        "normalized_sql": prepared.normalized_sql,
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
