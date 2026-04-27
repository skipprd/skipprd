use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::probe_target::ProbeTarget;
use crate::providers::{CatalogProvider, DatasetCatalogProvider, DatasetId};
use react_core::agent::AgentCtx;
use react_core::tools::Tool;

pub struct SqlStatsTool {
    pub catalog: Option<Arc<dyn CatalogProvider>>,
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
}

#[async_trait]
impl Tool for SqlStatsTool {
    fn name(&self) -> &'static str {
        "sql_stats"
    }
    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let raw_table = args
            .get("table")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let field = args
            .get("field")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if raw_table.is_empty() {
            return Ok(serde_json::json!({
                "ok": false,
                "error": "probe_policy_invalid_target",
                "code": "missing_table",
                "hint": "Provide args.table as fully-qualified <catalog>.<database>.<table>."
            }));
        }
        let target = match ProbeTarget::from_table_and_field(ctx, &raw_table, Some(&field)) {
            Ok(t) => t,
            Err(code) => {
                return Ok(serde_json::json!({
                    "ok": false,
                    "error": "probe_policy_invalid_target",
                    "code": code,
                    "table": raw_table,
                    "hint": "Provide args.table as fully-qualified <catalog>.<database>.<table>."
                }));
            }
        };
        let table = target.table_fqn();
        if field.is_empty() {
            return Ok(serde_json::json!({
                "ok": false,
                "error": "probe_policy_invalid_target",
                "code": "missing_field",
                "table": table,
                "hint": "sql_stats requires args.field. Call sql_schema(args:{table}) first and pick a concrete field."
            }));
        }

        // Probe policy: table/field must resolve from canonical catalog or provider schema.
        let mut known_fields: Option<Vec<String>> = None;
        if let Some(cat) = self.catalog.as_ref() {
            if let Ok(Some(c)) = cat.read_catalog(ctx.scope(), &table).await {
                let cols: Vec<String> = c.fields.iter().map(|f| f.name.clone()).collect();
                if !cols.is_empty() {
                    known_fields = Some(cols);
                }
            }
        }
        if known_fields.is_none() {
            if let Some(dsprov) = self.datasets.as_ref() {
                if let Ok(ds) = DatasetId::parse_fqn_strict(&table) {
                    if let Ok(cols) = dsprov.get_dataset_schema(&ds).await {
                        let names: Vec<String> = cols.into_iter().map(|(n, _)| n).collect();
                        if !names.is_empty() {
                            known_fields = Some(names);
                        }
                    }
                }
            }
        }
        let Some(known_fields) = known_fields else {
            return Ok(serde_json::json!({
                "ok": false,
                "error": "probe_policy_invalid_target",
                "code": "unknown_table",
                "table": table
            }));
        };
        if !known_fields.iter().any(|f| f == &field) {
            return Ok(serde_json::json!({
                "ok": false,
                "error": "probe_policy_invalid_target",
                "code": "unknown_field",
                "table": table,
                "field": field,
                "known_fields": known_fields.into_iter().take(80).collect::<Vec<_>>()
            }));
        }

        // Try catalog first (acts as cache)
        let mut distinct: Option<u64> = None;
        let mut min_numeric: Option<f64> = None;
        let mut max_numeric: Option<f64> = None;
        let mut max_len: Option<u64> = None;
        let mut nulls: u64 = 0;
        if let Some(cat) = self.catalog.as_ref() {
            if let Ok(Some(c)) = cat.read_catalog(ctx.scope(), &table).await {
                for f in c.fields.iter() {
                    if f.name == field {
                        if let Some(st) = f.stats.as_ref() {
                            distinct = st.approx_distinct;
                            min_numeric = st.min_numeric;
                            max_numeric = st.max_numeric;
                            max_len = st.max_len;
                            nulls = st.nulls;
                        }
                        break;
                    }
                }
            }
        }

        if distinct.is_none()
            && min_numeric.is_none()
            && max_numeric.is_none()
            && max_len.is_none()
            && nulls == 0
        {
            // Optional fallback to provider stats (if available)
            if let Some(dsprov) = self.datasets.as_ref() {
                if let Ok(ds) = DatasetId::parse_fqn_strict(&table) {
                    if let Ok((ns_stats, _ds_stats)) = dsprov.get_dataset_stats(&ds, 80).await {
                        if let Some(fs) = ns_stats.fields.get(&field) {
                            distinct = fs.approx_distinct;
                            min_numeric = fs.min_numeric;
                            max_numeric = fs.max_numeric;
                            max_len = fs.max_len;
                            nulls = fs.nulls;
                        }
                    }
                }
            }
        }

        if distinct.is_none()
            && min_numeric.is_none()
            && max_numeric.is_none()
            && max_len.is_none()
            && nulls == 0
        {
            return Ok(
                serde_json::json!({"ok": false, "error": "no stats in catalog (and provider stats unavailable)"}),
            );
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
