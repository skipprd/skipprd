use async_trait::async_trait;
use serde_json::Value;
use crate::agent::AgentCtx;
use crate::tools::Tool;
use std::sync::Arc;
use crate::providers::{CatalogProvider, DatasetCatalogProvider, DatasetId};

pub struct SqlStatsTool {
    pub catalog: Option<Arc<dyn CatalogProvider>>,
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
}

#[async_trait]
impl Tool for SqlStatsTool {
    fn name(&self) -> &'static str { "sql_stats" }
    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let table = args.get("table").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
        let field = args.get("field").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
        if table.is_empty() || field.is_empty() {
            return Ok(serde_json::json!({"ok": false, "error": "missing table/field"}));
        }

        // Try catalog first (acts as cache)
        let mut distinct: Option<u64> = None;
        let mut min_numeric: Option<f64> = None;
        let mut max_numeric: Option<f64> = None;
        let mut max_len: Option<u64> = None;
        let mut nulls: u64 = 0;
        if let Some(cat) = self.catalog.as_ref() {
            if let Ok(Some(c)) = cat.read_catalog(&ctx.scope, &table).await {
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

        if distinct.is_none() && min_numeric.is_none() && max_numeric.is_none() && max_len.is_none() && nulls == 0 {
            // Optional fallback to provider stats (if available)
            if let Some(dsprov) = self.datasets.as_ref() {
                let ds = parse_dataset_id_strict(&table)?;
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

        if distinct.is_none() && min_numeric.is_none() && max_numeric.is_none() && max_len.is_none() && nulls == 0 {
            return Ok(serde_json::json!({"ok": false, "error": "no stats in catalog (and provider stats unavailable)"}));
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

fn parse_dataset_id_strict(s: &str) -> Result<DatasetId, String> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 3 {
        return Err("table must be fully-qualified <catalog>.<database>.<table> for provider stats fallback".to_string());
    }
    Ok(DatasetId {
        catalog: parts[0].to_string(),
        database: parts[1].to_string(),
        table: parts[2].to_string(),
    })
}


