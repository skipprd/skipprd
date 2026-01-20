use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::info;

use react_core::agent::AgentCtx;
use crate::dbt::remediate::active_provider_dialect;
use crate::data_engineer::schema_yml;
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

fn staging_model_name_simple(expected_table: &str) -> String {
    format!("stg_{}", sanitize_ident(expected_table))
}

fn staging_model_name_scoped(expected_db: &str, expected_table: &str) -> String {
    format!(
        "stg_{}__{}",
        sanitize_ident(expected_db),
        sanitize_ident(expected_table)
    )
}

/// Resolve a collision-free staging model name for a dataset.
///
/// Strategy (in priority order):
/// - If a canonical `stg_<table>` already exists, reuse it (stable upgrades).
/// - Else if a scoped `stg_<schema>__<table>` already exists, reuse it (back-compat).
/// - Else prefer `stg_<table>` unless it would collide in this call (already used) or in the
///   existing project. If it would collide, use `stg_<schema>__<table>`.
/// - As a last resort (pathological collisions), append a numeric suffix.
fn resolve_staging_model_name(
    expected_db: &str,
    expected_table: &str,
    existing_names: &HashSet<String>,
    used_names: &HashSet<String>,
) -> String {
    let simple = staging_model_name_simple(expected_table);
    let scoped = staging_model_name_scoped(expected_db, expected_table);

    // Reuse existing models where possible to keep behavior stable over time.
    if existing_names.contains(&simple) && !used_names.contains(&simple) {
        return simple;
    }
    if existing_names.contains(&scoped) && !used_names.contains(&scoped) {
        return scoped;
    }

    // Prefer simple name unless it would collide.
    let simple_collides = used_names.contains(&simple) || existing_names.contains(&simple);
    let scoped_collides = used_names.contains(&scoped) || existing_names.contains(&scoped);

    if !simple_collides {
        return simple;
    }
    if !scoped_collides {
        return scoped;
    }

    // Last resort: ensure uniqueness by suffixing.
    for i in 2..=99usize {
        let cand = format!("{}_{}", scoped, i);
        if !used_names.contains(&cand) && !existing_names.contains(&cand) {
            return cand;
        }
    }
    scoped
}

fn staging_model_rel_path_for_name(model_name: &str) -> String {
    format!("models/staging/{}.sql", model_name)
}

fn strip_jinja_macros_in_sql_comments(sql: &str) -> (String, usize) {
    // DBT/Jinja can evaluate macros even inside SQL comments depending on adapter parsing.
    // We defensively strip comment segments that contain Jinja tokens.
    let mut removed = 0usize;

    // 1) Remove line comments that contain Jinja tokens
    let mut lines_out: Vec<String> = Vec::new();
    for line in sql.lines() {
        let t = line.trim_start();
        let is_line_comment = t.starts_with("--");
        let has_jinja = t.contains("{{") || t.contains("{%") || t.contains("}}") || t.contains("%}");
        if is_line_comment && has_jinja {
            removed += 1;
            continue;
        }
        lines_out.push(line.to_string());
    }
    let mut s = lines_out.join("\n");

    // 2) Remove block comments that contain Jinja tokens (best-effort, non-nested)
    loop {
        let Some(start) = s.find("/*") else { break };
        let Some(end_rel) = s[start + 2..].find("*/") else { break };
        let end = start + 2 + end_rel + 2;
        let block = &s[start..end];
        let has_jinja = block.contains("{{") || block.contains("{%") || block.contains("}}") || block.contains("%}");
        if has_jinja {
            removed += 1;
            s.replace_range(start..end, "");
            continue;
        }
        // Skip past this block and continue scanning after it.
        let after = end.min(s.len());
        let rest = s[after..].to_string();
        let mut prefix = s[..after].to_string();
        // Move scan window: replace s with rest, but keep prefix in an accumulator-like way.
        // Simpler: break out and do a second pass without removing non-jinja blocks.
        // (we already handled jinja blocks above)
        prefix.push_str(&rest);
        s = prefix;
        break;
    }

    (s, removed)
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

fn normalize_jinja_config_tags(sql: &str) -> String {
    // dbt config is a macro call, not a Jinja tag. Convert common mistaken usage:
    //   {% config(...) %}  ->  {{ config(...) }}
    let mut out: Vec<String> = Vec::new();
    for line in sql.lines() {
        let t = line.trim();
        if t.starts_with("{%") && t.contains("config(") && t.ends_with("%}") {
            let mut s = line.to_string();
            s = s.replacen("{%", "{{", 1);
            // Replace only the final closing token on this line.
            if let Some(pos) = s.rfind("%}") {
                s.replace_range(pos..pos + 2, "}}");
            }
            out.push(s);
            continue;
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

fn strip_all_config_lines(sql: &str) -> String {
    // Remove config-only lines; we will inject a canonical single config line at top.
    let mut out: Vec<String> = Vec::new();
    for line in sql.lines() {
        let t = line.trim();
        let is_config_line = (t.starts_with("{{") || t.starts_with("{%")) && t.contains("config(");
        if is_config_line {
            continue;
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

fn ensure_model_config(sql: &str, schema_suffix: &str, alias: &str) -> String {
    let sql = normalize_jinja_config_tags(sql);
    let body = strip_all_config_lines(&sql).trim().to_string();
    if body.is_empty() {
        return format!("{{{{ config(schema=\"{}\", alias=\"{}\") }}}}\n", schema_suffix, alias);
    }
    format!(
        "{{{{ config(schema=\"{}\", alias=\"{}\") }}}}\n\n{}",
        schema_suffix, alias, body
    )
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
        let _ = dbt.ensure_minimal_project(&ctx.scope).await;

        // Keep sources in models/schema.yml (monotonic merge; never overwrite existing sources).
        let base = ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string();
        let schema_key = format!("{}/models/schema.yml", base);
        let existing_schema: Option<String> = match ctx.storage.get_bytes(&schema_key).await {
            Ok(bytes) => Some(String::from_utf8_lossy(&bytes).to_string()),
            Err(_) => None,
        };
        let merged = match schema_yml::merge_sources_yaml(existing_schema.as_deref(), &dataset_ids) {
            Ok(s) => s,
            Err(e) => {
                // Preserve the prior content if it existed but was not parseable/mergeable.
                if let Some(prev) = existing_schema.as_ref() {
                    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
                    let backup_key = format!("{}/models/schema.yml.bak.{}.yml", base, ts);
                    let _ = ctx
                        .storage
                        .put_bytes(&backup_key, prev.as_bytes(), "text/yaml")
                        .await;
                }
                // Fall back to canonical sources for the requested dataset_ids only.
                let canonical = schema_yml::sources_yaml_from_dataset_ids(&dataset_ids)?;
                info!(target: "staging_model", "models/schema.yml merge failed; wrote canonical sources only");
                // Include original merge error to aid debugging.
                let _ = e;
                canonical
            }
        };
        let _ = ctx
            .storage
            .put_bytes(&schema_key, merged.as_bytes(), "text/yaml")
            .await;

        let dialect = crate::config::resolved_config_from_ctx(ctx)
            .map(active_provider_dialect)
            .unwrap_or_else(|| "Unknown SQL dialect".to_string());

        // Suffix strategy: set dbt model `schema` to the SILVER suffix (not the full schema name),
        // so dbt materializes into <DBT_TARGET_SCHEMA>_<silver_suffix>.
        let silver_db = crate::config::resolved_config_from_ctx(ctx)
            .map(|c| c.providers.dbt.naming.silver_suffix.clone())
            .unwrap_or_else(|| "silver".to_string());

        // Discover existing staging model names to keep naming stable across runs.
        let staging_prefix = format!("{}/models/staging/", base);
        let mut existing_model_names: HashSet<String> = HashSet::new();
        if let Ok(keys) = ctx.storage.list_prefix(&staging_prefix).await {
            for k in keys {
                if !k.ends_with(".sql") {
                    continue;
                }
                if k.contains("/_versions/") {
                    continue;
                }
                let file = k.rsplit('/').next().unwrap_or("").trim();
                let stem = file.strip_suffix(".sql").unwrap_or(file).trim();
                if !stem.is_empty() {
                    existing_model_names.insert(stem.to_string());
                }
            }
        }
        let mut used_model_names: HashSet<String> = HashSet::new();

        let mut written: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();

        // Optional fast-path: direct write mode. If SQL is provided, we write exactly one dataset's model
        // without calling the LLM (avoids extra OpenAI calls and reduces timeout risk).
        if let Some(mut sql_out) = resolve_direct_sql(&args) {
            if dataset_ids.len() != 1 {
                return Err("staging_model direct-write requires exactly one dataset (use a single-item args.dataset_ids).".to_string());
            }
            let ds = &dataset_ids[0];
            let (_cat, expected_db, expected_table) = parse_dataset_id(ds)
                .ok_or_else(|| format!("invalid dataset_id '{ds}' (expected <catalog>.<schema>.<table>)"))?;
            let model_name = resolve_staging_model_name(&expected_db, &expected_table, &existing_model_names, &used_model_names);
            let rel_path = staging_model_rel_path_for_name(&model_name);
            let key = format!("{}/{}", base, rel_path);

            // Force a canonical config: schema suffix + alias (prevents dbt identifier collisions).
            sql_out = ensure_model_config(&sql_out, &silver_db, &model_name);

            let (sql_sanitized, removed) = strip_jinja_macros_in_sql_comments(&sql_out);
            if removed > 0 {
                sql_out = sql_sanitized;
            }
            let has_any_source = sql_out.to_lowercase().contains("source(");
            if !has_any_source {
                return Err(format!(
                    "staging_model requires using a dbt source(). Expected to read from: {{ source(\"{expected_db}\", \"{expected_table}\") }} (derived from dataset_id {ds})."
                ));
            }
            if !contains_expected_source_call(&sql_out, &expected_db, &expected_table) {
                return Err(format!(
                    "staging_model produced a source() call that does not match the expected source/table for dataset_id {ds}. Expected: {{ source(\"{expected_db}\", \"{expected_table}\") }}."
                ));
            }
            ctx.storage.put_bytes(&key, sql_out.as_bytes(), "text/sql").await?;
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
            let model_name = resolve_staging_model_name(&expected_db, &expected_table, &existing_model_names, &used_model_names);
            used_model_names.insert(model_name.clone());
            let rel_path = staging_model_rel_path_for_name(&model_name);
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
                 - IMPORTANT: this model must be built into the SILVER schema suffix \"{silver_db}\".\n\
                   Include a dbt config block that sets schema to \"{silver_db}\".\n\
                 - IMPORTANT: Do NOT set a dbt alias. The suite will enforce a canonical alias to avoid collisions.\n\
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

            let mut sql_out = parsed.sql;
            // Force a canonical config: schema suffix + alias (prevents dbt identifier collisions).
            sql_out = ensure_model_config(&sql_out, &silver_db, &model_name);

            let (sql_sanitized, _removed) = strip_jinja_macros_in_sql_comments(&sql_out);
            sql_out = sql_sanitized;
            let has_any_source = sql_out.to_lowercase().contains("source(");
            if !has_any_source {
                return Err(format!(
                    "staging_model requires using a dbt source(). Expected to read from: {{ source(\"{expected_db}\", \"{expected_table}\") }} (derived from dataset_id {ds})."
                ));
            }
            if !contains_expected_source_call(&sql_out, &expected_db, &expected_table) {
                return Err(format!(
                    "staging_model produced a source() call that does not match the expected source/table for dataset_id {ds}. Expected: {{ source(\"{expected_db}\", \"{expected_table}\") }}."
                ));
            }

            ctx.storage.put_bytes(&key, sql_out.as_bytes(), "text/sql").await?;
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
    fn strip_jinja_macros_in_line_comment() {
        let sql = "-- {{ source('x','y') }}\nselect 1";
        let (out, removed) = strip_jinja_macros_in_sql_comments(sql);
        assert_eq!(removed, 1);
        assert!(out.contains("select 1"));
        assert!(!out.contains("source('x'"));
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
    fn resolve_staging_model_name_reuses_existing_simple() {
        let existing = HashSet::from(["stg_raw_orders".to_string()]);
        let used = HashSet::new();
        let got = resolve_staging_model_name("test_raw", "raw_orders", &existing, &used);
        assert_eq!(got, "stg_raw_orders");
    }

    #[test]
    fn resolve_staging_model_name_defaults_to_simple_when_no_collision() {
        let existing = HashSet::new();
        let used = HashSet::new();
        let got = resolve_staging_model_name("test_raw", "raw_orders", &existing, &used);
        assert_eq!(got, "stg_raw_orders");
    }

    #[test]
    fn resolve_staging_model_name_falls_back_to_scoped_on_collision() {
        let existing = HashSet::from(["stg_raw_orders".to_string()]);
        let used = HashSet::from(["stg_raw_orders".to_string()]);
        let got = resolve_staging_model_name("test_raw", "raw_orders", &existing, &used);
        assert_eq!(got, "stg_test_raw__raw_orders");
    }
}