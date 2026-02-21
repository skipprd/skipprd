use serde_json::Value;
use std::collections::HashMap;

use react_core::agent::AgentCtx;
use react_core::llm::{ChatMessage, LlmCallOptions, LlmExpectedFormat};

fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key).ok().and_then(|s| s.trim().parse::<u32>().ok())
}

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok().and_then(|s| s.trim().parse::<usize>().ok())
}

pub fn sql_first_max_output_tokens(default: u32) -> u32 {
    // Keep bounded; in OpenAI Responses, output_tokens includes reasoning tokens.
    env_u32("REACT_SQL_FIRST_MAX_OUTPUT_TOKENS")
        .unwrap_or(default)
        .max(800)
        .min(16_000)
}

pub fn sql_first_max_repair_attempts(default: usize) -> usize {
    env_usize("REACT_SQL_FIRST_MAX_REPAIR_ATTEMPTS")
        .unwrap_or(default)
        .max(1)
        .min(8)
}

fn parse_json_object_lenient(text: &str) -> Result<Value, String> {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return Ok(v);
    }
    let s = text.trim();
    let st = s.find('{').ok_or_else(|| "no '{' found".to_string())?;
    let en = s.rfind('}').ok_or_else(|| "no '}' found".to_string())?;
    if en <= st {
        return Err("invalid brace span".to_string());
    }
    serde_json::from_str::<Value>(&s[st..=en]).map_err(|e| e.to_string())
}

pub fn reject_non_sql_surface(sql: &str) -> Result<(), String> {
    // SQL-first hard cutover: the draft MUST be plain SQL; no Jinja/macros.
    let t = sql.trim();
    if t.is_empty() {
        return Err("sql_first: sql is empty".to_string());
    }
    let low = t.to_ascii_lowercase();
    if low.contains("{{") || low.contains("}}") {
        return Err("sql_first: draft SQL must not contain Jinja delimiters '{{' / '}}'".to_string());
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
                let repl = format!(
                    "select {} from {}",
                    select_exprs.join(", "),
                    placeholder
                );
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
    temperature: f32,
) -> Result<SqlFirstDraft, String> {
    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: system,
        },
        ChatMessage {
            role: "user".to_string(),
            content: user_json,
        },
    ];
    let opts = LlmCallOptions {
        prompt_id,
        thread_id: ctx.thread_id.clone(),
        expected_format: LlmExpectedFormat::JsonObject,
        temperature: Some(temperature),
        top_p: Some(1.0),
        max_output_tokens: Some(max_output_tokens),
        reasoning_effort: None,
    };
    let raw = ctx.llm.chat(&messages, &opts).map_err(|e| e.to_string())?;
    let v = parse_json_object_lenient(&raw)?;
    let sql = v
        .get("sql")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if sql.is_empty() {
        return Err("sql_first: missing required field 'sql'".to_string());
    }
    let notes = v
        .get("notes")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .take(40)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(SqlFirstDraft { sql, notes })
}

pub async fn validate_sql_quick(
    ctx: &AgentCtx,
    sql: &str,
    placeholder_replacements: &HashMap<String, String>,
) -> Result<(), String> {
    reject_non_sql_surface(sql)?;
    let expanded = apply_placeholders(sql, placeholder_replacements);
    if let Some(msg) = ctx.warehouse.unsupported_sql_reason(&expanded) {
        return Err(msg);
    }
    let probe = wrap_sql_for_validation(&expanded, 1);
    ctx.warehouse.query(&probe).await.map(|_| ()).map_err(|e| e)
}

