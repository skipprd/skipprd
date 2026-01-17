use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::info;

use crate::agent::AgentCtx;
use crate::dbt::remediate::active_provider_dialect;
use crate::llm::ChatMessage;
use crate::providers::DatasetCatalogProvider;
use crate::tools::Tool;

#[derive(Clone)]
pub struct StagingModelTool {
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
}

fn sanitize_ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let ok = ch.is_ascii_alphanumeric() || ch == '_';
        out.push(if ok { ch.to_ascii_lowercase() } else { '_' });
    }
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    out.trim_matches('_').to_string()
}

fn parse_dataset_id(dataset_id: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = dataset_id.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((parts[0].to_string(), parts[1].to_string(), parts[2].to_string()))
}

fn staging_model_rel_path_for_dataset(dataset_id: &str) -> Option<String> {
    let (_cat, db, table) = parse_dataset_id(dataset_id)?;
    let name = format!("stg_{}__{}", sanitize_ident(&db), sanitize_ident(&table));
    Some(format!("models/staging/{}.sql", name))
}

fn make_sources_yaml_from_dataset_ids(dataset_ids: &[String]) -> String {
    // Keep sources in ONE place: models/schema.yml.
    // Strategy: group by (catalog, database), emit each table under that source.
    let mut out = String::new();
    out.push_str("version: 2\n\n");
    out.push_str("sources:\n");

    let mut by_cat_db: std::collections::BTreeMap<(String, String), Vec<String>> = std::collections::BTreeMap::new();
    for ds in dataset_ids {
        if let Some((cat, db, table)) = parse_dataset_id(ds) {
            by_cat_db.entry((cat, db)).or_default().push(table);
        }
    }
    for ((cat, db), mut tables) in by_cat_db {
        tables.sort();
        tables.dedup();
        out.push_str(&format!("  - name: {}\n", db));
        out.push_str(&format!("    database: {}\n", cat));
        out.push_str(&format!("    schema: {}\n", db));
        out.push_str("    tables:\n");
        for t in tables {
            out.push_str(&format!("      - name: {}\n", t));
        }
    }
    out
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct LlmStagingResponse {
    #[serde(default)]
    sql: String,
    #[serde(default)]
    notes: Vec<String>,
}

fn parse_json_from_llm(text: &str) -> Result<Value, String> {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return Ok(v);
    }
    let s = text.trim();
    let start = s.find('{').ok_or_else(|| "LLM response did not contain JSON object".to_string())?;
    let end = s.rfind('}').ok_or_else(|| "LLM response did not contain JSON object".to_string())?;
    if end <= start {
        return Err("LLM response JSON object bounds invalid".to_string());
    }
    serde_json::from_str::<Value>(&s[start..=end]).map_err(|e| e.to_string())
}

#[async_trait]
impl Tool for StagingModelTool {
    fn name(&self) -> &'static str {
        "staging_model"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let query = ctx.query.as_ref().ok_or_else(|| "query provider missing".to_string())?;
        let dbt = ctx.dbt.as_ref().ok_or_else(|| "dbt provider missing".to_string())?;

        // Resolve dataset_ids: explicit list or all discovered.
        let mut dataset_ids: Vec<String> = args
            .get("dataset_ids")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if dataset_ids.is_empty() {
            let ds_provider = self
                .datasets
                .as_ref()
                .ok_or_else(|| "datasets provider missing (cannot default to all datasets)".to_string())?;
            let ds = ds_provider.list_datasets().await?;
            dataset_ids = ds.into_iter().map(|d| d.fqn()).collect();
        }

        dataset_ids.sort();
        dataset_ids.dedup();

        let instructions = args
            .get("instructions")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        // Ensure minimal dbt project exists before writing artifacts.
        let _ = dbt.ensure_minimal_project(&ctx.scope).await;

        // Keep sources in models/schema.yml (deterministic).
        let schema_yml = make_sources_yaml_from_dataset_ids(&dataset_ids);
        let base = ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string();
        let schema_key = format!("{}/models/schema.yml", base);
        let _ = ctx.storage.put_bytes(&schema_key, schema_yml.as_bytes(), "text/yaml").await;

        let dialect = ctx
            .resolved_config
            .as_ref()
            .map(|c| active_provider_dialect(c.as_ref()))
            .unwrap_or_else(|| "Unknown SQL dialect".to_string());

        let mut written: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();

        for ds in dataset_ids.iter() {
            let Some(rel_path) = staging_model_rel_path_for_dataset(ds) else { continue };
            let key = format!("{}/{}", base, rel_path);

            let cols = query.schema(ds).await.unwrap_or_default();
            let cols_json: Vec<Value> = cols
                .iter()
                .map(|(n, t)| serde_json::json!({"name": n, "type": t}))
                .collect();

            let sys = format!(
                "You are an expert analytics engineer.\n\
                 Task: author a dbt *staging/silver* model for ONE source dataset.\n\
                 Dialect: {dialect}\n\
                 Requirements:\n\
                 - Output MUST be valid JSON only.\n\
                 - Produce a dbt model SQL SELECT that reads from the dbt source for this table.\n\
                 - This is SILVER: include sensible cleansing/normalization and stable column naming.\n\
                 - Nested fields: use Trino/Athena struct dereference like context.session.id (DO NOT quote the whole path).\n\
                 - If a column name is reserved (e.g. timestamp), quote JUST the identifier (\"timestamp\").\n\
                 - Keep changes aligned with the user's instructions, even if they are unconventional.\n\
                 Output schema:\n\
                 {{\"sql\":\"...\",\"notes\":[\"...\"]}}\n"
            );

            let user = serde_json::json!({
                "dataset_id": ds,
                "schema_columns": cols_json,
                "user_instructions": instructions,
                "expected_model_path": rel_path,
            })
            .to_string();

            let resp_text = ctx
                .llm
                .chat(&[
                    ChatMessage { role: "system".to_string(), content: sys },
                    ChatMessage { role: "user".to_string(), content: user },
                ])
                .map_err(|e| format!("staging_model LLM call failed: {}", e))?;

            let v = parse_json_from_llm(&resp_text)?;
            let parsed: LlmStagingResponse =
                serde_json::from_value(v).map_err(|e| format!("failed to parse staging_model JSON: {}", e))?;

            if parsed.sql.trim().is_empty() {
                continue;
            }

            ctx.storage.put_bytes(&key, parsed.sql.as_bytes(), "text/sql").await?;
            written.push(key);
            for n in parsed.notes {
                if !n.trim().is_empty() {
                    notes.push(format!("{}: {}", ds, n));
                }
            }
        }

        // Minimal, not chatty
        info!(
            target: "staging_model",
            datasets = dataset_ids.len(),
            written = written.len(),
            "staging_model finished"
        );

        // Dedup notes to keep response bounded
        let mut seen: HashSet<String> = HashSet::new();
        let mut out_notes: Vec<String> = Vec::new();
        for n in notes {
            if seen.insert(n.clone()) {
                out_notes.push(n);
            }
            if out_notes.len() >= 50 {
                break;
            }
        }

        Ok(serde_json::json!({
            "ok": true,
            "datasets": dataset_ids.len(),
            "written_keys": written,
            "schema_key": schema_key,
            "notes": out_notes,
        }))
    }
}

