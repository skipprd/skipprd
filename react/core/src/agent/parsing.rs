use serde_json::Value;

use super::{Agent, AgentStepTypeV1, AgentStepV1, FinalEnvelope, ParsedStep, SchemaId};

impl Agent {
    /// Some model backends emit "JSON-like" text with literal control characters (e.g. raw newlines)
    /// inside string values. That is invalid JSON and `serde_json` will reject it.
    ///
    /// This function repairs ONLY those invalid characters inside string literals by escaping them.
    /// It is intentionally conservative: it does not try to fix other kinds of malformed JSON.
    fn escape_control_chars_in_json_strings(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 8);
        let mut in_str = false;
        let mut esc = false;
        for ch in s.chars() {
            if in_str {
                if esc {
                    // Preserve whatever was escaped (including escaped newlines like \n).
                    out.push(ch);
                    esc = false;
                    continue;
                }
                if ch == '\\' {
                    out.push(ch);
                    esc = true;
                    continue;
                }
                match ch {
                    // Escape raw control characters that are illegal in JSON strings.
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    '\u{08}' => out.push_str("\\b"),
                    '\u{0C}' => out.push_str("\\f"),
                    '"' => {
                        out.push(ch);
                        in_str = false;
                    }
                    c if (c as u32) < 0x20 => {
                        // Any remaining control chars -> \u00XX
                        out.push_str(&format!("\\u{:04x}", c as u32));
                    }
                    _ => out.push(ch),
                }
                continue;
            }

            // Not in string
            if esc {
                out.push(ch);
                esc = false;
                continue;
            }
            match ch {
                '"' => {
                    out.push(ch);
                    in_str = true;
                }
                '\\' => {
                    // Outside strings this is still meaningful JSON (e.g. escapes in whitespace-less JSON5-ish),
                    // but we preserve it.
                    out.push(ch);
                    esc = true;
                }
                _ => out.push(ch),
            }
        }
        out
    }

    fn strip_markdown_code_fences(raw: &str) -> String {
        let t = raw.trim();
        if !t.starts_with("```") {
            return t.to_string();
        }
        // Handle ```json ... ``` and ``` ... ```
        let t = t
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim();
        if let Some(end) = t.rfind("```") {
            return t[..end].trim().to_string();
        }
        t.to_string()
    }

    pub(crate) fn parse_agent_step(raw: &str) -> Result<ParsedStep, String> {
        let cleaned = Self::strip_markdown_code_fences(raw);
        let trimmed = cleaned.trim();

        fn parse_json_from_model_string_field(raw_json: &str, what: &str) -> Result<Value, String> {
            match serde_json::from_str::<Value>(raw_json) {
                Ok(v) => Ok(v),
                Err(e) => {
                    // Repair raw control chars inside JSON strings (literal newlines, etc).
                    let repaired = Agent::escape_control_chars_in_json_strings(raw_json);
                    serde_json::from_str::<Value>(&repaired)
                        .map_err(|_| format!("{what} is not valid JSON string: {e}"))
                }
            }
        }

        let v = match serde_json::from_str::<Value>(trimmed) {
            Ok(v) => v,
            Err(e) => {
                // Conservative repair: escape control chars inside strings (raw newlines, etc).
                let repaired = Self::escape_control_chars_in_json_strings(trimmed);
                serde_json::from_str::<Value>(&repaired)
                    .map_err(|_| format!("invalid JSON from model: {}", e))?
            }
        };

        crate::schema_registry::validate(SchemaId::AgentStepV1, &v)?;
        let step: AgentStepV1 = serde_json::from_value::<AgentStepV1>(v).map_err(|e| {
            format!(
                "failed to deserialize {}: {}",
                SchemaId::AgentStepV1.name(),
                e
            )
        })?;

        match step.type_ {
            AgentStepTypeV1::Tool => {
                let Some(name) = step.name else {
                    return Err("agent.step.v1 validation error: missing tool name".to_string());
                };
                let Some(args_json) = step.args else {
                    return Err("agent.step.v1 validation error: missing tool args".to_string());
                };
                if step.final_.is_some() {
                    return Err(
                        "agent.step.v1 validation error: tool step must not include final".to_string(),
                    );
                }
                let args: Value = parse_json_from_model_string_field(
                    &args_json,
                    "agent.step.v1 validation error: args",
                )?;
                Ok(ParsedStep::Tool { name, args })
            }
            AgentStepTypeV1::Final => {
                if step.name.is_some() || step.args.is_some() {
                    return Err(
                        "agent.step.v1 validation error: final step must not include name/args"
                            .to_string(),
                    );
                }
                let Some(fin) = step.final_ else {
                    return Err("agent.step.v1 validation error: missing final".to_string());
                };
                let payload: Value = parse_json_from_model_string_field(
                    &fin.payload,
                    "agent.step.v1 validation error: final.payload",
                )?;
                Ok(ParsedStep::Final {
                    final_env: FinalEnvelope {
                        kind: fin.kind,
                        payload,
                        display: fin.display,
                    },
                })
            }
        }
    }

    fn extract_all_json_values(s: &str, max: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if max == 0 {
            return out;
        }

        let mut i = 0usize;
        while i < s.len() && out.len() < max {
            // Find the next start.
            let mut start: Option<usize> = None;
            for (off, ch) in s[i..].char_indices() {
                if ch == '{' || ch == '[' {
                    start = Some(i + off);
                    break;
                }
            }
            let Some(st) = start else { break };

            // Walk forward from start until we close the top-level value.
            let mut stack: Vec<char> = Vec::new();
            let mut in_str = false;
            let mut esc = false;
            let mut end: Option<usize> = None;
            for (pos, ch) in s[st..].char_indices() {
                let abs = st + pos;

                if stack.is_empty() {
                    stack.push(ch);
                } else if in_str {
                    if esc {
                        esc = false;
                        continue;
                    }
                    if ch == '\\' {
                        esc = true;
                        continue;
                    }
                    if ch == '"' {
                        in_str = false;
                    }
                    continue;
                } else {
                    match ch {
                        '"' => in_str = true,
                        '{' | '[' => stack.push(ch),
                        '}' => {
                            if matches!(stack.pop(), Some('{')) && stack.is_empty() {
                                end = Some(abs);
                                break;
                            }
                        }
                        ']' => {
                            if matches!(stack.pop(), Some('[')) && stack.is_empty() {
                                end = Some(abs);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }

            let Some(en) = end else { break };
            out.push(s[st..=en].to_string());
            i = en + 1;
        }

        out
    }
}
