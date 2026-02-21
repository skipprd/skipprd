use serde_json::Value;
use std::collections::HashMap;

use react_core::agent::AgentCtx;
use react_core::llm::{ChatMessage, LlmCallOptions, LlmExpectedFormat};

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

pub fn strip_trailing_semicolon(sql: &str) -> String {
    let mut s = sql.trim().to_string();
    while s.ends_with(';') {
        s.pop();
        s = s.trim_end().to_string();
    }
    s
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
    let expanded = apply_placeholders(sql, placeholder_replacements);
    if let Some(msg) = ctx.warehouse.unsupported_sql_reason(&expanded) {
        return Err(msg);
    }
    let probe = wrap_sql_for_validation(&expanded, 1);
    ctx.warehouse.query(&probe).await.map(|_| ()).map_err(|e| e)
}

