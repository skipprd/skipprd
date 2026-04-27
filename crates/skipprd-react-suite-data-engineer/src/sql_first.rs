use serde::Deserialize;
use std::collections::HashMap;

use react_core::agent::AgentCtx;
use react_core::llm::{ChatMessage, ChatRole, LlmCallOptions, LlmExpectedFormat};

pub fn sql_first_max_output_tokens(default: u32) -> u32 {
    super::env_util::sql_first_max_output_tokens(default)
}

pub fn sql_first_max_repair_attempts(default: usize) -> usize {
    super::env_util::sql_first_max_repair_attempts(default)
}

#[derive(serde::Serialize, Deserialize, schemars::JsonSchema)]
struct SqlFirstDraftPayload {
    sql: String,
    #[serde(default)]
    notes: Vec<String>,
}

pub fn reject_non_sql_surface(sql: &str) -> Result<(), String> {
    // SQL-first hard cutover: the draft MUST be plain SQL; no Jinja/macros.
    let t = sql.trim();
    if t.is_empty() {
        return Err("sql_first: sql is empty".to_string());
    }
    let low = t.to_ascii_lowercase();
    if low.contains("{{") || low.contains("}}") {
        return Err(
            "sql_first: draft SQL must not contain Jinja delimiters '{{' / '}}'".to_string(),
        );
    }
    // Disallow dbt-only macros/functions in the SQL-first surface.
    for bad in ["ref(", "source(", "doc("] {
        if low.contains(bad) {
            return Err(format!(
                "sql_first: draft SQL must not call '{}'; use placeholders only",
                bad.trim_end_matches('(')
            ));
        }
    }
    Ok(())
}

pub fn strip_trailing_semicolon(sql: &str) -> String {
    let mut s = sql.trim().to_string();
    while s.ends_with(';') {
        s.pop();
        s = s.trim_end().to_string();
    }
    s
}

pub fn expand_select_star_from_placeholder(
    sql: &str,
    placeholder: &str,
    select_exprs: &[String],
) -> Option<String> {
    if select_exprs.is_empty() {
        return None;
    }
    // Normalize whitespace so we can do simple substring matching without a SQL parser.
    let norm = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let low = norm.to_ascii_lowercase();
    let ph = placeholder.to_ascii_lowercase();

    // Pattern A: SELECT * FROM <placeholder> ...
    let pat = format!("select * from {}", ph);
    if let Some(idx) = low.rfind(&pat) {
        let repl = format!("select {} from {}", select_exprs.join(", "), placeholder);
        let out = format!("{}{}{}", &norm[..idx], repl, &norm[idx + pat.len()..]);
        return Some(out);
    }

    // Pattern B: SELECT <alias>.* FROM <placeholder> <alias> ...
    let from_pat = format!("from {} ", ph);
    if let Some(fidx) = low.rfind(&from_pat) {
        let after = &low[fidx + from_pat.len()..];
        let alias = after.split_whitespace().next().unwrap_or("").trim();
        if !alias.is_empty() {
            let pat2 = format!("select {}.* from {}", alias, ph);
            if let Some(idx2) = low.rfind(&pat2) {
                let repl = format!("select {} from {}", select_exprs.join(", "), placeholder);
                let out = format!("{}{}{}", &norm[..idx2], repl, &norm[idx2 + pat2.len()..]);
                return Some(out);
            }
        }
    }
    None
}

pub fn apply_placeholders(sql: &str, replacements: &HashMap<String, String>) -> String {
    let mut out = sql.to_string();
    for (k, v) in replacements.iter() {
        out = out.replace(k, v);
    }
    out
}

fn normalize_output_column_name(name: &str) -> String {
    let t = name.trim().trim_matches('"').trim_matches('`').trim();
    t.to_ascii_lowercase()
}

fn duplicate_output_columns(header: &[String]) -> Vec<String> {
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut first_seen: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    for h in header.iter() {
        let norm = normalize_output_column_name(h);
        if norm.is_empty() {
            continue;
        }
        *counts.entry(norm.clone()).or_insert(0) += 1;
        first_seen
            .entry(norm)
            .or_insert_with(|| h.trim().to_string());
    }
    counts
        .into_iter()
        .filter_map(|(k, n)| if n > 1 { Some(k) } else { None })
        .map(|k| first_seen.get(&k).cloned().unwrap_or(k))
        .collect()
}

pub fn detect_duplicate_output_columns(header: &[String]) -> Vec<String> {
    duplicate_output_columns(header)
}

pub fn wrap_sql_for_validation(sql: &str, limit: usize) -> String {
    let cleaned = strip_trailing_semicolon(sql);
    // Avoid double LIMIT guessing. Wrapping is the most robust across dialects.
    format!("SELECT * FROM (\n{cleaned}\n) __react_sql_first__ LIMIT {limit}")
}

#[derive(Clone, Debug)]
pub struct SqlFirstDraft {
    pub sql: String,
    pub notes: Vec<String>,
}

pub async fn llm_draft_sql_json(
    ctx: &AgentCtx,
    system: String,
    user_json: String,
    prompt_id: &'static str,
    max_output_tokens: u32,
    reasoning_effort: react_core::llm::ReasoningEffort,
) -> Result<SqlFirstDraft, String> {
    let messages = vec![
        ChatMessage {
            role: ChatRole::System,
            content: system,
        },
        ChatMessage {
            role: ChatRole::User,
            content: user_json,
        },
    ];
    let schema = react_core::schema_registry::OpenAiStrictSchema::for_type::<SqlFirstDraftPayload>(
        "data_engineer.sql_first_draft",
    )
    .map_err(|e| e.to_string())?;

    let opts = LlmCallOptions {
        prompt_id,
        thread_id: ctx.thread_id().clone(),
        model: None,
        expected_format: LlmExpectedFormat::JsonSchema(schema),
        max_output_tokens: Some(max_output_tokens),
        reasoning_effort: Some(reasoning_effort),
        ..Default::default()
    };
    let payload: SqlFirstDraftPayload = ctx
        .llm_chat_json(&messages, &opts)
        .await
        .map_err(|e| e.to_string())?;
    let sql = payload.sql.trim().to_string();
    if sql.is_empty() {
        return Err("sql_first: missing required field 'sql'".to_string());
    }
    let notes = payload
        .notes
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .take(40)
        .collect::<Vec<_>>();
    Ok(SqlFirstDraft { sql, notes })
}

pub async fn validate_sql_quick(
    ctx: &AgentCtx,
    sql: &str,
    placeholder_replacements: &HashMap<String, String>,
) -> Result<(), String> {
    reject_non_sql_surface(sql)?;
    let wh = crate::ctx_ext::actx_warehouse(ctx)
        .ok_or_else(|| "warehouse provider missing".to_string())?;
    let expanded = apply_placeholders(sql, placeholder_replacements);
    let probe = wrap_sql_for_validation(&expanded, 1);
    let res = crate::transient_retry::retry_transient_default("validate_sql_quick", || async {
        wh.query(&probe).await
    })
    .await?;
    let dups = duplicate_output_columns(&res.header);
    if !dups.is_empty() {
        return Err(format!(
            "duplicate_output_columns: query projects duplicate output column name(s): {}. \
Use unique aliases and avoid patterns like `SELECT *` plus re-defining an existing column in the same projection.",
            dups.join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::duplicate_output_columns;

    #[test]
    fn duplicate_output_columns_is_case_insensitive_and_quote_tolerant() {
        let header = vec![
            "order_id".to_string(),
            "\"Order_ID\"".to_string(),
            "customer_id".to_string(),
            "  `customer_id`  ".to_string(),
            "placed_at".to_string(),
        ];
        let dups = duplicate_output_columns(&header);
        assert_eq!(
            dups,
            vec!["customer_id".to_string(), "order_id".to_string()]
        );
    }
}
