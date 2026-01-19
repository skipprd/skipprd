use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use react_core::agent::AgentCtx;
use react_core::providers::{CatalogProvider, DatasetCatalogProvider, QueryProvider};
use react_core::tools::Tool;

pub struct SqlSchemaTool {
    pub query: Arc<dyn QueryProvider>,
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
    pub catalog: Option<Arc<dyn CatalogProvider>>,
}

#[async_trait]
impl Tool for SqlSchemaTool {
    fn name(&self) -> &'static str { "sql_schema" }
    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let table_opt = args.get("table").and_then(|x| x.as_str()).map(|s| s.trim().to_string());
        if let Some(t) = table_opt {
            if let Some(cat) = self.catalog.as_ref() {
                if let Ok(Some(c)) = cat.read_catalog(&ctx.scope, &t).await {
                    let cols: Vec<Value> = c
                        .fields
                        .iter()
                        .map(|f| serde_json::json!({"name": f.name, "type": f.data_type.clone().unwrap_or_default()}))
                        .collect();
                    if !cols.is_empty() {
                        return Ok(serde_json::json!({"ok": true, "columns": cols, "source": "catalog"}));
                    }
                }
            }
            match self.query.schema(&t).await {
                Ok(cols) => Ok(serde_json::json!({"ok": true, "columns": cols.into_iter().map(|(n,t)| serde_json::json!({"name": n, "type": t})).collect::<Vec<_>>(), "source": "provider"})),
                Err(e) => Ok(serde_json::json!({"ok": false, "error": e})),
            }
        } else {
            // List datasets from provider if supported
            if let Some(ds) = self.datasets.as_ref() {
                match ds.list_datasets().await {
                    Ok(items) => {
                        let names: Vec<String> = items.into_iter().map(|d| d.fqn()).collect();
                        Ok(serde_json::json!({"ok": true, "tables": names}))
                    }
                    Err(e) => Ok(serde_json::json!({"ok": false, "error": e})),
                }
            } else {
                Ok(serde_json::json!({"ok": false, "error": "table required"}))
            }
        }
    }
}

