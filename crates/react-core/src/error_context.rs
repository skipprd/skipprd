use std::collections::BTreeMap;

use serde_json::Value;

use crate::agent::AgentCtx;
use crate::session::ToolObservation;

/// Build a prompt-safe error context string for a failed tool call.
///
/// Policy:
/// - If the full error payload fits in `max_chars`, return it (no truncation).
/// - Otherwise, return a deterministic excerpt composed of keyword-window matches plus head/tail.
pub fn render_failure_context(observation: &ToolObservation, max_chars: usize) -> String {
    let max_chars = max_chars.max(256);
    let full = build_error_blob(observation);
    if full.chars().count() <= max_chars {
        return full;
    }
    excerpt_by_keywords(&full, max_chars, &default_error_markers())
}

/// Determine the maximum number of characters to allocate for error context.
///
/// Order:
/// - `LLM_MAX_PROMPT_CHARS` (explicit override)
/// - `LLM_CONTEXT_LENGTH` (tokens) * 4, minus a safety margin
/// - default: 32_000 chars
pub fn estimate_max_prompt_chars(_ctx: &AgentCtx) -> usize {
    if let Ok(v) = std::env::var("LLM_MAX_PROMPT_CHARS") {
        if let Ok(n) = v.trim().parse::<usize>() {
            if n >= 1024 {
                return n;
            }
        }
    }

    let ctx_tokens = std::env::var("LLM_CONTEXT_LENGTH")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok());
    if let Some(toks) = ctx_tokens {
        // Very conservative conversion: ~4 chars/token, with a reserved safety margin for the rest of the prompt.
        let est = toks.saturating_mul(4);
        // Leave room for system prompt, tool card, prior transcript, and the model response.
        return est.saturating_sub(8_000).max(8_000);
    }

    32_000
}

/// Deterministic excerpt: keyword-window matches (+ head/tail) under a char budget.
pub fn excerpt_by_keywords(text: &str, max_chars: usize, patterns: &[&str]) -> String {
    let max_chars = max_chars.max(256);
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return trim_to_chars(text, max_chars);
    }

    // Collect windows around matched lines.
    let mut ranges: Vec<(usize, usize)> = Vec::new(); // [start, end) line indices
    for (i, line) in lines.iter().enumerate() {
        let ll = line.to_ascii_lowercase();
        if patterns.iter().any(|p| !p.is_empty() && ll.contains(p)) {
            let start = i.saturating_sub(3);
            let end = (i + 9).min(lines.len()); // +8 lines after
            ranges.push((start, end));
        }
    }

    // Always include head/tail if we have budget.
    let head_n = 10usize.min(lines.len());
    let tail_n = 10usize.min(lines.len());
    ranges.push((0, head_n));
    if lines.len() > tail_n {
        ranges.push((lines.len() - tail_n, lines.len()));
    }

    // Merge overlaps deterministically.
    ranges.sort_by_key(|(s, e)| (*s, *e));
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in ranges {
        if s >= e {
            continue;
        }
        if let Some((ms, me)) = merged.last_mut() {
            if s <= *me {
                *me = (*me).max(e);
                *ms = (*ms).min(s);
                continue;
            }
        }
        merged.push((s, e));
    }

    // Build output until we hit budget.
    let mut out = String::new();
    let mut included_lines: usize = 0;
    for (s, e) in merged {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("--- excerpt lines {}..{} ---\n", s + 1, e));
        for i in s..e {
            let line = lines[i];
            if !line.trim().is_empty() {
                out.push_str(line);
                out.push('\n');
            } else {
                out.push('\n');
            }
            included_lines += 1;
            if out.chars().count() >= max_chars {
                let marker = format!(
                    "\n...omitted (excerpt truncated; max_chars={}, included_lines={}, total_lines={})",
                    max_chars, included_lines, lines.len()
                );
                let marker_len = marker.chars().count();
                let keep = max_chars.saturating_sub(marker_len).max(0);
                let mut trimmed = trim_to_chars(&out, keep);
                trimmed.push_str(&marker);
                return trimmed;
            }
        }
    }

    // If we still somehow exceeded budget, trim.
    trim_to_chars(&out, max_chars)
}

fn default_error_markers() -> Vec<&'static str> {
    vec![
        "error",
        "exception",
        "panic",
        "traceback",
        "failed",
        "failure",
        "fatal",
        "timeout",
        "denied",
        "permission",
        "cannot",
        "not found",
        "invalid",
        "mismatched",
        "expected",
        "line ",
        "at ",
    ]
}

fn build_error_blob(observation: &ToolObservation) -> String {
    let mut parts: Vec<String> = Vec::new();

    if !observation.errors.is_empty() {
        parts.push("errors:".to_string());
        for e in observation.errors.iter() {
            if !e.trim().is_empty() {
                parts.push(e.to_string());
            }
        }
    }

    // Generic string extraction from extra:
    // - capture string leaves under common keys when present (stdout/stderr/logs/message/detail/trace).
    // - otherwise, fall back to pretty JSON of extra.
    let extra_v = Value::Object(
        observation
            .extra
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<serde_json::Map<String, Value>>(),
    );

    let mut extracted: BTreeMap<String, String> = BTreeMap::new();
    extract_common_string_fields(&extra_v, &mut extracted, "", 0, 6);
    if !extracted.is_empty() {
        parts.push("extra:".to_string());
        for (k, v) in extracted {
            if v.trim().is_empty() {
                continue;
            }
            parts.push(format!("-- {k} --"));
            parts.push(v);
        }
    } else if !observation.extra.is_empty() {
        if let Ok(s) = serde_json::to_string_pretty(&extra_v) {
            parts.push("extra_json:".to_string());
            parts.push(s);
        }
    }

    if parts.is_empty() {
        // Last resort: serialize whole observation (including ok/errors/warnings) without losing information.
        let mut env = serde_json::Map::new();
        env.insert("ok".to_string(), Value::Bool(observation.ok));
        env.insert(
            "errors".to_string(),
            Value::Array(observation.errors.iter().map(|s| Value::String(s.clone())).collect()),
        );
        env.insert(
            "warnings".to_string(),
            Value::Array(observation.warnings.iter().map(|s| Value::String(s.clone())).collect()),
        );
        env.insert("extra".to_string(), extra_v);
        return serde_json::to_string_pretty(&Value::Object(env)).unwrap_or_else(|_| "{}".to_string());
    }

    parts.join("\n")
}

fn extract_common_string_fields(
    v: &Value,
    out: &mut BTreeMap<String, String>,
    path: &str,
    depth: usize,
    max_depth: usize,
) {
    if depth > max_depth {
        return;
    }
    match v {
        Value::String(s) => {
            // Record only if path indicates this is likely error/log text (or if we were asked explicitly).
            let p = path.to_ascii_lowercase();
            let interesting = p.contains("stderr")
                || p.contains("stdout")
                || p.contains("logs")
                || p.contains("trace")
                || p.contains("message")
                || p.contains("detail")
                || p.contains("error");
            if interesting && !s.trim().is_empty() {
                out.insert(path.to_string(), s.clone());
            }
        }
        Value::Object(m) => {
            for (k, vv) in m.iter() {
                let next = if path.is_empty() { k.clone() } else { format!("{}.{}", path, k) };
                extract_common_string_fields(vv, out, &next, depth + 1, max_depth);
            }
        }
        Value::Array(arr) => {
            for (i, vv) in arr.iter().enumerate() {
                let next = if path.is_empty() { format!("[{}]", i) } else { format!("{}[{}]", path, i) };
                extract_common_string_fields(vv, out, &next, depth + 1, max_depth);
            }
        }
        _ => {}
    }
}

fn trim_to_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut out = String::new();
    out.reserve(max_chars.min(s.len()));
    for (i, ch) in s.chars().enumerate() {
        if i >= max_chars {
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn excerpt_is_bounded_and_includes_keyword_window() {
        let mut text = String::new();
        for i in 0..200 {
            text.push_str(&format!("line {}\n", i));
        }
        text.push_str("this is a PANIC: something bad happened\n");
        for i in 200..500 {
            text.push_str(&format!("tail {}\n", i));
        }

        let out = excerpt_by_keywords(&text, 600, &["panic"]);
        assert!(out.to_ascii_lowercase().contains("panic"));
        assert!(out.chars().count() <= 600);
    }

    #[test]
    fn render_failure_context_includes_md5_varbinary_line_when_it_fits() {
        let mut extra = BTreeMap::new();
        extra.insert(
            "logs".to_string(),
            json!({
                "run_or_build": {
                    "stdout": "Failure in model fct_onboarding_steps (models/core/fct_onboarding_steps.sql)\nline 67:9: Unexpected parameters (varchar) for function md5. Expected: md5(varbinary)\ncompiled code at target/compiled/...\n"
                }
            }),
        );
        let obs = ToolObservation {
            ok: false,
            errors: vec!["Runtime Error in model fct_onboarding_steps (models/core/fct_onboarding_steps.sql)".to_string()],
            warnings: vec![],
            extra,
        };

        let rendered = render_failure_context(&obs, 10_000);
        assert!(rendered.contains("md5(varbinary)"));
    }

    #[test]
    fn render_failure_context_excerpts_when_too_large() {
        let big = "ok\n".repeat(50_000) + "ERROR: important\n" + &"more\n".repeat(50_000);
        let obs = ToolObservation {
            ok: false,
            errors: vec![big],
            warnings: vec![],
            extra: BTreeMap::new(),
        };
        let rendered = render_failure_context(&obs, 2000);
        assert!(rendered.to_ascii_lowercase().contains("error"));
        assert!(rendered.chars().count() <= 2000);
    }
}

