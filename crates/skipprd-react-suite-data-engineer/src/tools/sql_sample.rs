use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::probe_target::ProbeTarget;
use crate::providers::QueryProvider;
use crate::references::ColumnRef;
use react_core::agent::AgentCtx;
use react_core::tools::Tool;

pub struct SqlSampleTool {
    pub query: Arc<dyn QueryProvider>,
}

#[async_trait]
impl Tool for SqlSampleTool {
    fn name(&self) -> &'static str {
        "sql_sample"
    }
    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let table = args.get("table").and_then(|x| x.as_str()).unwrap_or("");
        let field = args.get("field").and_then(|x| x.as_str()).unwrap_or("");
        let k = args.get("k").and_then(|x| x.as_u64()).unwrap_or(10);
        if table.trim().is_empty() {
            return Ok(serde_json::json!({
                "ok": false,
                "error": "probe_policy_invalid_target",
                "code": "missing_table",
                "hint": "Provide args.table as fully-qualified <catalog>.<database>.<table>."
            }));
        }
        let target = match ProbeTarget::from_table_and_field(ctx, table, Some(field)) {
            Ok(t) => t,
            Err(code) => {
                return Ok(serde_json::json!({
                    "ok": false,
                    "error": "probe_policy_invalid_target",
                    "code": code,
                    "table": table,
                    "hint": "Provide args.table as fully-qualified <catalog>.<database>.<table>."
                }));
            }
        };
        let table = target.table_fqn();
        let Some(field) = target.field.as_deref() else {
            return Ok(serde_json::json!({
                "ok": false,
                "error": "probe_policy_invalid_target",
                "code": "missing_field",
                "table": table,
                "hint": "sql_sample requires args.field. Call sql_schema(args:{table}) first and pick a field. For row samples use run_sql LIMIT."
            }));
        };
        if ColumnRef::new(target.dataset.clone(), field).is_none() {
            return Ok(serde_json::json!({
                "ok": false,
                "error": "probe_policy_invalid_target",
                "code": "invalid_field",
                "table": table,
                "field": field,
            }));
        }
        match crate::transient_retry::retry_transient_default("sql_sample_schema", || async {
            self.query.schema(&table).await
        })
        .await
        {
            Ok(cols) => {
                let names: Vec<String> = cols.into_iter().map(|(n, _)| n).collect();
                if !names.iter().any(|n| n == field) {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "error": "probe_policy_invalid_target",
                        "code": "unknown_field",
                        "table": table,
                        "field": field,
                        "known_fields": names.into_iter().take(80).collect::<Vec<_>>()
                    }));
                }
            }
            Err(_) => {
                return Ok(serde_json::json!({
                    "ok": false,
                    "error": "probe_policy_invalid_target",
                    "code": "unknown_table",
                    "table": table
                }));
            }
        }
        let sql = format!(
            "SELECT {f} AS value, COUNT(1) AS cnt FROM {t} GROUP BY {f} ORDER BY cnt DESC LIMIT {k}",
            f = field,
            t = table,
            k = k
        );
        match crate::transient_retry::retry_transient_default("sql_sample_query", || async {
            self.query.query(&sql).await
        })
        .await
        {
            Ok(qr) => {
                let mut values: Vec<Value> = Vec::new();
                for r in qr.rows {
                    let v = r.get(0).cloned().unwrap_or_default();
                    let c = r.get(1).cloned().unwrap_or_default();
                    let cnt = c.parse::<u64>().unwrap_or(0);
                    values.push(serde_json::json!({"value": v, "count": cnt}));
                }
                Ok(serde_json::json!({"ok": true, "values": values, "meta": qr.meta}))
            }
            Err(e) => Ok(serde_json::json!({"ok": false, "error": e})),
        }
    }
}
