use serde_json::Value;

use super::{Agent, AgentCtx};

pub(crate) fn title_case_words(s: &str) -> String {
    let mut out = String::new();
    for (i, w) in s.split_whitespace().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut chars = w.chars();
        if let Some(c) = chars.next() {
            out.extend(c.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

pub(crate) fn clean_tool_name(name: &str, args: &Value) -> String {
    match name {
        "file" => {
            let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
            match op {
                "get" => {
                    let p = args
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim();
                    if !p.is_empty() {
                        return format!("Read {p}");
                    }
                    "Read file".to_string()
                }
                "list" => {
                    let p = args
                        .get("prefix")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim();
                    if !p.is_empty() {
                        return format!("List {p}");
                    }
                    "List files".to_string()
                }
                "get_json" => {
                    let p = args
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim();
                    if !p.is_empty() {
                        return format!("Read JSON {p}");
                    }
                    "Read JSON".to_string()
                }
                "patch" => {
                    let p = args
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim();
                    if !p.is_empty() {
                        return format!("Patch {p}");
                    }
                    "Patch file".to_string()
                }
                _ => {
                    if !op.is_empty() {
                        return format!("file {op}");
                    }
                    "file".to_string()
                }
            }
        }
        "run_sql" => "Run SQL".to_string(),
        "sql_schema" => {
            let t = args
                .get("table")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if !t.is_empty() {
                format!("Describe {t}")
            } else {
                "List tables".to_string()
            }
        }
        "sql_stats" => {
            let t = args
                .get("table")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if !t.is_empty() {
                format!("Stats {t}")
            } else {
                "Stats".to_string()
            }
        }
        "sql_sample" => {
            let t = args
                .get("table")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if !t.is_empty() {
                format!("Sample {t}")
            } else {
                "Sample".to_string()
            }
        }
        "vect_query" => "Vector search".to_string(),
        other => title_case_words(&other.replace('_', " ")),
    }
}

impl Agent {
    pub(crate) fn gen_uuid() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    pub(crate) fn transcript_add(
        transcript: &mut Vec<String>,
        line: String,
        tx: &Option<tokio::sync::mpsc::UnboundedSender<String>>,
    ) {
        if let Some(t) = tx.as_ref() {
            let _ = t.send(line.clone());
        }
        transcript.push(line);
    }

    fn trim_transcript_for_prompt(transcript: &mut Vec<String>) {
        // Hard cutover: do NOT replay the full raw transcript to the model.
        // Keep a stable head (System/Tools/Prelude/User) plus a bounded tail of recent context.
        const MAX_HEAD_LINES: usize = 24;
        const MAX_TAIL_LINES: usize = 18;
        const MAX_LINE_CHARS: usize = 2_500;

        // Truncate overly-large single lines (tool outputs can be huge).
        for l in transcript.iter_mut() {
            if l.chars().count() > MAX_LINE_CHARS {
                let mut t = l.chars().take(MAX_LINE_CHARS).collect::<String>();
                t.push_str(" …(truncated)");
                *l = t;
            }
        }

        // Find head end (inclusive of the first User: line).
        let head_end = transcript
            .iter()
            .position(|l| l.starts_with("User:"))
            .map(|i| i + 1)
            .unwrap_or(transcript.len());

        let mut head: Vec<String> = transcript.iter().take(head_end).cloned().collect();
        if head.len() > MAX_HEAD_LINES {
            head.truncate(MAX_HEAD_LINES);
        }

        let rest: &[String] = if head_end <= transcript.len() {
            &transcript[head_end..]
        } else {
            &[]
        };
        let tail_n = MAX_TAIL_LINES.min(rest.len());
        let mut tail: Vec<String> = rest
            .iter()
            .skip(rest.len().saturating_sub(tail_n))
            .cloned()
            .collect();

        head.append(&mut tail);
        *transcript = head;
    }

    pub(crate) fn prompt_from_transcript(
        ctx: &AgentCtx,
        transcript: &mut Vec<String>,
        output_contract_line: &str,
    ) -> String {
        Self::trim_transcript_for_prompt(transcript);

        // Enforce a best-effort max prompt size budget by dropping tail lines.
        let max_prompt_chars = crate::error_context::estimate_max_prompt_chars(ctx).max(1024);
        loop {
            let used_chars: usize = transcript.iter().map(|l| l.chars().count() + 1).sum::<usize>()
                + output_contract_line.chars().count()
                + 1;
            if used_chars <= max_prompt_chars {
                if let Some(tx) = ctx.trace_tx.as_ref() {
                    let _ = tx.send(format!(
                        "prompt_size chars={} lines={} budget={}",
                        used_chars,
                        transcript.len(),
                        max_prompt_chars
                    ));
                }
                return format!("{}\n{}", transcript.join("\n"), output_contract_line);
            }
            // Drop the most recent tail line if possible.
            if transcript.len() <= 4 {
                // Can't trim further without destroying head; return anyway.
                return format!("{}\n{}", transcript.join("\n"), output_contract_line);
            }
            transcript.pop();
        }
    }
}
