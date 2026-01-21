use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::info;

use react_core::agent::AgentCtx;
use crate::data_engineer::dbt_repair::remediate::active_provider_dialect;
use crate::data_engineer::project_fs;
use react_core::llm::ChatMessage;
use react_core::providers::DatasetCatalogProvider;
use react_core::tools::Tool;

fn extract_string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn resolve_dataset_ids(args: &Value) -> Result<Vec<String>, String> {
    // Required shape: dataset_ids: [ ... ]
    let mut out: Vec<String> = args
        .get("dataset_ids")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    out.sort();
    out.dedup();
    if out.is_empty() {
        return Err(
            "staging_model requires args.dataset_ids (string[]) where each item is <catalog>.<schema>.<table>. Refusing to default to all datasets."
                .to_string(),
        );
    }
    // Validate format early to avoid silent no-ops.
    for ds in out.iter() {
        if parse_dataset_id(ds).is_none() {
            return Err(format!(
                "invalid dataset_id '{ds}'. Expected <catalog>.<schema>.<table> (e.g. AwsDataCatalog.test_raw.raw_customers)."
            ));
        }
    }
    Ok(out)
}

fn resolve_instructions(args: &Value) -> String {
    // Prefer "instructions", but accept common aliases used in prompts/logs.
    extract_string_arg(args, "instructions")
        .or_else(|| extract_string_arg(args, "user_instructions"))
        .unwrap_or_default()
}

fn resolve_direct_sql(args: &Value) -> Option<String> {
    // Accept common keys used by agents/prompts.
    extract_string_arg(args, "sql")
        .or_else(|| extract_string_arg(args, "staging_model"))
        .or_else(|| extract_string_arg(args, "expression"))
}

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

/// Canonical, deterministic staging model name for a dataset_id <catalog>.<schema>.<table>.
///
/// We own the output DBs and require a strict 1:1 mapping, so this is intentionally stable:
/// `stg_<schema>_<table>` (single underscore join, both sides sanitized).
fn canonical_staging_model_name(expected_db: &str, expected_table: &str) -> String {
    format!(
        "stg_{}_{}",
        sanitize_ident(expected_db),
        sanitize_ident(expected_table)
    )
}

fn staging_model_rel_path_for_name(model_name: &str) -> String {
    format!("models/staging/{}.sql", model_name)
}

fn contains_expected_source_call(sql: &str, expected_db: &str, expected_table: &str) -> bool {
    let lc = sql.to_lowercase();
    let db = expected_db.to_lowercase();
    let table = expected_table.to_lowercase();
    // Common spellings; keep intentionally simple and robust.
    let patterns = [
        format!("source('{}','{}')", db, table),
        format!("source('{}', '{}')", db, table),
        format!("source(\"{}\",\"{}\")", db, table),
        format!("source(\"{}\", \"{}\")", db, table),
    ];
    patterns.iter().any(|p| lc.contains(p))
}

fn matching_staging_rel_paths_by_source(
    staging_files: &[(String, String)],
    expected_db: &str,
    expected_table: &str,
) -> Vec<String> {
    staging_files
        .iter()
        .filter_map(|(rel_path, content)| {
            if contains_expected_source_call(content, expected_db, expected_table) {
                Some(rel_path.clone())
            } else {
                None
            }
        })
        .collect()
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

        // Resolve datasets explicitly; NEVER default to all datasets.
        let dataset_ids = resolve_dataset_ids(&args)?;

        let instructions = resolve_instructions(&args);

        // Ensure minimal dbt project exists before writing artifacts.
        if let Err(e) = dbt.ensure_minimal_project(&ctx.scope).await {
            return Ok(serde_json::json!({
                "ok": false,
                "datasets": dataset_ids.len(),
                "written_keys": [],
                "schema_key": Value::Null,
                "notes": [],
                "errors": [format!("failed to ensure minimal dbt project: {e}")],
            }));
        }

        // Ensure models/schema.yml exists; patch pipeline will deterministically rebuild sources.
        let base = ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string();
        let schema_rel = "models/schema.yml".to_string();
        let schema_key = format!("{}/{}", base, schema_rel);
        let existing_schema: Option<String> = match ctx.storage.get_bytes(&schema_key).await {
            Ok(bytes) => Some(String::from_utf8_lossy(&bytes).to_string()),
            Err(_) => None,
        };
        let seed = existing_schema.clone().unwrap_or_else(|| "version: 2\n".to_string());
        let patch_text = project_fs::create_patch_text(existing_schema.as_deref().unwrap_or(""), &seed);
        let outcome = match project_fs::apply_patch(
            ctx,
            self.datasets.as_ref(),
            &schema_rel,
            &patch_text,
            None,
            true,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => {
                return Ok(serde_json::json!({
                    "ok": false,
                    "datasets": dataset_ids.len(),
                    "written_keys": [],
                    "schema_key": schema_key,
                    "notes": [],
                    "errors": [format!("failed to patch models/schema.yml: {e}")],
                }));
            }
        };
        if let Err(e) = ctx.storage.put_bytes(&schema_key, outcome.content.as_bytes(), "text/yaml").await {
            return Ok(serde_json::json!({
                "ok": false,
                "datasets": dataset_ids.len(),
                "written_keys": [],
                "schema_key": schema_key,
                "notes": [],
                "errors": [format!("failed to write models/schema.yml: {e}")],
            }));
        }

        let dialect = crate::config::resolved_config_from_ctx(ctx)
            .map(active_provider_dialect)
            .unwrap_or_else(|| "Unknown SQL dialect".to_string());

        // Discover existing staging model files so we can update by semantic identity (source()),
        // not by filename (prevents duplicate staging models for the same dataset).
        let staging_prefix = format!("{}/models/staging/", base);
        let mut staging_files: Vec<(String, String)> = Vec::new(); // (rel_path, content)
        let mut unreadable_staging_rel_paths: Vec<String> = Vec::new();
        if let Ok(keys) = ctx.storage.list_prefix(&staging_prefix).await {
            for k in keys {
                if !k.ends_with(".sql") {
                    continue;
                }
                if k.contains("/_versions/") {
                    continue;
                }
                // Convert storage key -> project-relative path (models/staging/<name>.sql)
                let rel_path = k
                    .strip_prefix(&(base.clone() + "/"))
                    .unwrap_or(k.as_str())
                    .to_string();
                match ctx.storage.get_bytes(&k).await {
                    Ok(bytes) => {
                        let content = String::from_utf8_lossy(&bytes).to_string();
                        staging_files.push((rel_path, content));
                    }
                    Err(_) => {
                        unreadable_staging_rel_paths.push(rel_path);
                    }
                }
            }
        }

        let mut written: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        // Optional fast-path: direct write mode. If SQL is provided, we write exactly one dataset's model
        // without calling the LLM (avoids extra OpenAI calls and reduces timeout risk).
        if let Some(sql_out) = resolve_direct_sql(&args) {
            if dataset_ids.len() != 1 {
                return Err("staging_model direct-write requires exactly one dataset (use a single-item args.dataset_ids).".to_string());
            }
            let ds = &dataset_ids[0];
            let (_cat, expected_db, expected_table) = parse_dataset_id(ds)
                .ok_or_else(|| format!("invalid dataset_id '{ds}' (expected <catalog>.<schema>.<table>)"))?;
            let canonical_name = canonical_staging_model_name(&expected_db, &expected_table);

            let matches = matching_staging_rel_paths_by_source(&staging_files, &expected_db, &expected_table);
            if matches.len() > 1 {
                errors.push(format!(
                    "{ds}: multiple staging models reference the same source({expected_db},{expected_table}); refusing to write. Conflicts: {:?}",
                    matches
                ));
                return Ok(serde_json::json!({
                    "ok": false,
                    "datasets": dataset_ids.len(),
                    "written_keys": written,
                    "schema_key": schema_key,
                    "notes": [],
                    "errors": errors,
                }));
            }
            if matches.is_empty() && !unreadable_staging_rel_paths.is_empty() {
                errors.push(format!(
                    "{ds}: unable to reliably determine whether an existing staging model already targets this source because some staging files were unreadable. Unreadable: {:?}",
                    unreadable_staging_rel_paths
                ));
                return Ok(serde_json::json!({
                    "ok": false,
                    "datasets": dataset_ids.len(),
                    "written_keys": written,
                    "schema_key": schema_key,
                    "notes": [],
                    "errors": errors,
                }));
            }
            let rel_path = matches
                .get(0)
                .cloned()
                .unwrap_or_else(|| staging_model_rel_path_for_name(&canonical_name));
            let key = format!("{}/{}", base, rel_path);

            let has_any_source = sql_out.to_lowercase().contains("source(");
            if !has_any_source {
                errors.push(format!(
                    "staging_model requires using a dbt source(). Expected to read from: {{ source(\"{expected_db}\", \"{expected_table}\") }} (derived from dataset_id {ds})."
                ));
                return Ok(serde_json::json!({
                    "ok": false,
                    "datasets": dataset_ids.len(),
                    "written_keys": written,
                    "schema_key": schema_key,
                    "notes": [],
                    "errors": errors,
                }));
            }
            if !contains_expected_source_call(&sql_out, &expected_db, &expected_table) {
                errors.push(format!(
                    "staging_model produced a source() call that does not match the expected source/table for dataset_id {ds}. Expected: {{ source(\"{expected_db}\", \"{expected_table}\") }}."
                ));
                return Ok(serde_json::json!({
                    "ok": false,
                    "datasets": dataset_ids.len(),
                    "written_keys": written,
                    "schema_key": schema_key,
                    "notes": [],
                    "errors": errors,
                }));
            }
            let existing = ctx
                .storage
                .get_bytes(&key)
                .await
                .ok()
                .map(|b| String::from_utf8_lossy(&b).to_string())
                .unwrap_or_default();
            let patch_text = project_fs::create_patch_text(&existing, &sql_out);
            let outcome = project_fs::apply_patch(ctx, None, &rel_path, &patch_text, None, true).await?;
            ctx.storage.put_bytes(&key, outcome.content.as_bytes(), "text/sql").await?;
            written.push(key);

            info!(
                target: "staging_model",
                datasets = dataset_ids.len(),
                written = written.len(),
                "staging_model finished (direct-write)"
            );

            return Ok(serde_json::json!({
                "ok": true,
                "datasets": dataset_ids.len(),
                "written_keys": written,
                "schema_key": schema_key,
                "notes": [],
            }));
        }

        for ds in dataset_ids.iter() {
            let (_cat, expected_db, expected_table) = parse_dataset_id(ds)
                .ok_or_else(|| format!("invalid dataset_id '{ds}' (expected <catalog>.<schema>.<table>)"))?;
            let canonical_name = canonical_staging_model_name(&expected_db, &expected_table);

            let matches = matching_staging_rel_paths_by_source(&staging_files, &expected_db, &expected_table);
            if matches.len() > 1 {
                errors.push(format!(
                    "{ds}: multiple staging models reference the same source({expected_db},{expected_table}); refusing to write. Conflicts: {:?}",
                    matches
                ));
                continue;
            }
            if matches.is_empty() && !unreadable_staging_rel_paths.is_empty() {
                errors.push(format!(
                    "{ds}: unable to reliably determine whether an existing staging model already targets this source because some staging files were unreadable. Unreadable: {:?}",
                    unreadable_staging_rel_paths
                ));
                continue;
            }

            // Exactly 1 match -> update in place (even if filename isn't canonical).
            // No matches -> create at canonical path.
            let rel_path = matches
                .get(0)
                .cloned()
                .unwrap_or_else(|| staging_model_rel_path_for_name(&canonical_name));
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
                 - CRITICAL: You MUST read from: FROM {{{{ source(\"{expected_db}\", \"{expected_table}\") }}}} (do not invent any other source name).\n\
                 - This is SILVER: include sensible cleansing/normalization and stable column naming.\n\
                 - Use the provided schema_columns types to guide casting and cleansing. Do NOT guess types from names.\n\
                 - Time-like fields MUST be detected from schema types when possible:\n\
                   - timestamp/datetime types: timestamp, timestamptz, datetime\n\
                   - date types: date\n\
                   If a field is string-typed but appears to encode time values, you may treat it as time-like only if schema_columns or samples strongly indicate it.\n\
                 - For any time-like field:\n\
                   - If the source is string-ish: create a `*_raw` expression using trim + nullif-empty so empty strings become NULL deterministically.\n\
                   - Produce the cleaned output field as a safe cast (Athena/Trino: try_cast(... as timestamp) or try_cast(... as date)).\n\
                   - Choose ONE explicitly:\n\
                     (A) Enforce non-null semantics by filtering rows where the cleaned field is NULL, OR\n\
                     (B) Keep NULLs and add a note recommending a conditional dbt test (with where:) and why.\n\
                 - IMPORTANT: Do NOT include a dbt config block or alias; the suite enforces canonical config/alias deterministically.\n\
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

            let v = match parse_json_from_llm(&resp_text) {
                Ok(v) => v,
                Err(e) => {
                    errors.push(format!("{ds}: failed to parse LLM JSON: {e}"));
                    continue;
                }
            };
            let parsed: LlmStagingResponse = match serde_json::from_value(v) {
                Ok(p) => p,
                Err(e) => {
                    errors.push(format!("{ds}: failed to parse staging_model JSON: {e}"));
                    continue;
                }
            };

            if parsed.sql.trim().is_empty() {
                errors.push(format!("{ds}: LLM returned empty sql"));
                continue;
            }

            let sql_out = parsed.sql;
            let has_any_source = sql_out.to_lowercase().contains("source(");
            if !has_any_source {
                errors.push(format!(
                    "{ds}: staging_model requires using a dbt source(). Expected: {{ source(\"{expected_db}\", \"{expected_table}\") }}."
                ));
                continue;
            }
            if !contains_expected_source_call(&sql_out, &expected_db, &expected_table) {
                errors.push(format!(
                    "{ds}: produced a source() call that does not match expected. Expected: {{ source(\"{expected_db}\", \"{expected_table}\") }}."
                ));
                continue;
            }

            let existing = ctx
                .storage
                .get_bytes(&key)
                .await
                .ok()
                .map(|b| String::from_utf8_lossy(&b).to_string())
                .unwrap_or_default();
            let patch_text = project_fs::create_patch_text(&existing, &sql_out);
            let outcome = project_fs::apply_patch(ctx, None, &rel_path, &patch_text, None, true).await?;
            ctx.storage.put_bytes(&key, outcome.content.as_bytes(), "text/sql").await?;
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
            "ok": errors.is_empty(),
            "datasets": dataset_ids.len(),
            "written_keys": written,
            "schema_key": schema_key,
            "notes": out_notes,
            "errors": errors,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_dataset_ids_accepts_dataset_ids_array() {
        let args = serde_json::json!({"dataset_ids":["AwsDataCatalog.test_raw.raw_orders","AwsDataCatalog.test_raw.raw_orders"]});
        let got = resolve_dataset_ids(&args).expect("ok");
        assert_eq!(got, vec!["AwsDataCatalog.test_raw.raw_orders".to_string()]);
    }

    #[test]
    fn resolve_dataset_ids_rejects_missing() {
        let args = serde_json::json!({});
        let err = resolve_dataset_ids(&args).unwrap_err();
        assert!(err.contains("requires args.dataset_ids"));
        assert!(err.contains("Refusing to default"));
    }

    #[test]
    fn resolve_dataset_ids_rejects_invalid_format() {
        let args = serde_json::json!({"dataset_ids":["test_raw__raw_customers"]});
        let err = resolve_dataset_ids(&args).unwrap_err();
        assert!(err.contains("invalid dataset_id"));
    }

    #[test]
    fn resolve_direct_sql_prefers_sql_then_staging_model_then_expression() {
        let a = serde_json::json!({"sql":"select 1"});
        assert_eq!(resolve_direct_sql(&a).as_deref(), Some("select 1"));

        let b = serde_json::json!({"staging_model":"select 2"});
        assert_eq!(resolve_direct_sql(&b).as_deref(), Some("select 2"));

        let c = serde_json::json!({"expression":"select 3"});
        assert_eq!(resolve_direct_sql(&c).as_deref(), Some("select 3"));
    }

    #[test]
    fn contains_expected_source_call_accepts_common_formats() {
        let sql1 = "select * from {{ source('test_raw','raw_orders') }}";
        let sql2 = "select * from {{ source(\"test_raw\", \"raw_orders\") }}";
        assert!(contains_expected_source_call(sql1, "test_raw", "raw_orders"));
        assert!(contains_expected_source_call(sql2, "test_raw", "raw_orders"));
        assert!(!contains_expected_source_call(sql2, "test_raw", "raw_customers"));
    }

    #[test]
    fn canonical_staging_model_name_is_deterministic_schema_table() {
        let got = canonical_staging_model_name("test_raw", "raw_orders");
        assert_eq!(got, "stg_test_raw_raw_orders");
        assert!(!got.contains("__"));
    }

    #[test]
    fn matching_staging_rel_paths_by_source_finds_exactly_one() {
        let files = vec![
            (
                "models/staging/stg_x.sql".to_string(),
                "select * from {{ source('test_raw','raw_orders') }}".to_string(),
            ),
            ("models/staging/stg_y.sql".to_string(), "select 1".to_string()),
        ];
        let got = matching_staging_rel_paths_by_source(&files, "test_raw", "raw_orders");
        assert_eq!(got, vec!["models/staging/stg_x.sql".to_string()]);
    }

    #[test]
    fn matching_staging_rel_paths_by_source_detects_duplicates() {
        let files = vec![
            (
                "models/staging/a.sql".to_string(),
                "select * from {{ source('test_raw','raw_orders') }}".to_string(),
            ),
            (
                "models/staging/b.sql".to_string(),
                "select * from {{ source(\"test_raw\", \"raw_orders\") }}".to_string(),
            ),
        ];
        let got = matching_staging_rel_paths_by_source(&files, "test_raw", "raw_orders");
        assert_eq!(got.len(), 2);
    }
}