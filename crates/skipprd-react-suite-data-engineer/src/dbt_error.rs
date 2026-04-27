#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbtErrorClass {
    DuplicateSources,
    ProfilesYaml,
    DbtProjectYaml,
    WarehouseConfig,
    /// dbt dependency graph is missing a declared source (e.g. schema.yml doesn't define it).
    ///
    /// This is a *grounding/truth* failure, not a SQL dialect issue. Treat as non-remediable by
    /// SQL patching; fix by reconciling `models/schema.yml` with actual dataset facts.
    MissingSource,
    /// Generic SQL compilation/runtime error (dialect mismatch or invalid SQL).
    ///
    /// This is intentionally provider-agnostic; the same class can be used to trigger remediation.
    SqlFailure,
    /// Fallback: SQL issues not confidently classified as `SqlFailure`.
    SqlOrModel,
    Unknown,
}

fn strip_ansi(s: &str) -> String {
    // Remove common ANSI escape sequences like "\x1b[0m".
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip ESC[...]m sequences.
            if let Some('[') = chars.peek().copied() {
                let _ = chars.next();
                while let Some(cc) = chars.next() {
                    if cc == 'm' {
                        break;
                    }
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Detect the common dbt contract error where `data_type` is missing from YAML column definitions.
///
/// This class of failure is remediated by patching YAML schema contracts, not by editing SQL.
#[cfg(test)]
pub fn logs_indicate_contract_data_type_missing(logs: &serde_json::Value) -> bool {
    let mut parts: Vec<&str> = Vec::new();
    for phase in ["compile", "run_or_build"] {
        for stream in ["stdout", "stderr"] {
            if let Some(s) = logs
                .get(phase)
                .and_then(|v| v.get(stream))
                .and_then(|v| v.as_str())
            {
                if !s.trim().is_empty() {
                    parts.push(s);
                }
            }
        }
    }
    if parts.is_empty() {
        return false;
    }
    let s = strip_ansi(&parts.join("\n"));
    let sl = s.to_lowercase();
    // Canonical dbt message (varies slightly by adapter/dbt version).
    if sl.contains("contracted models require data_type") {
        return true;
    }
    // Defensive heuristics.
    if sl.contains("data_type")
        && (sl.contains("yaml configuration") || sl.contains("within the yaml"))
        && sl.contains("column")
    {
        return true;
    }
    false
}

#[cfg(test)]
pub fn classify(errors: &[String]) -> DbtErrorClass {
    let joined = errors.join("\n");
    let s = crate::failure_text::normalize_text(&strip_ansi(&joined));

    if s.contains("dbt found two sources with the name") || s.contains("duplicate sources") {
        return DbtErrorClass::DuplicateSources;
    }
    if s.contains("got duplicate keys") && s.contains("map to \"database\"") {
        return DbtErrorClass::ProfilesYaml;
    }
    if s.contains("dbt encountered an error while trying to read your profiles.yml")
        || (s.contains("profiles.yml") && s.contains("syntax error near line"))
    {
        return DbtErrorClass::ProfilesYaml;
    }
    if s.contains("additional properties are not allowed") && s.contains("dbt_project.yml") {
        return DbtErrorClass::DbtProjectYaml;
    }
    if s.contains("could not find profile named") {
        return DbtErrorClass::ProfilesYaml;
    }
    if s.contains("depends on a source named") && s.contains("was not found") {
        return DbtErrorClass::MissingSource;
    }
    DbtErrorClass::Unknown
}

pub fn compact_brief(errors: &[String], max_errors: usize, max_chars_each: usize) -> String {
    if errors.is_empty() {
        return "dbt_validate failed with no error text.".to_string();
    }
    let mut lines: Vec<String> = Vec::new();
    for e in errors.iter().take(max_errors) {
        let clean = strip_ansi(e);
        let t = clean.trim();
        if t.is_empty() {
            continue;
        }
        if t.len() <= max_chars_each {
            lines.push(t.to_string());
        } else {
            lines.push(format!("{}…", &t[..max_chars_each]));
        }
    }
    if lines.is_empty() {
        return "dbt_validate failed (errors were empty after cleanup).".to_string();
    }
    lines.join("\n---\n")
}

#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema, PartialEq, Eq,
)]
pub struct DbtFailureSummary {
    /// Human-readable condensed summary (may be multi-line).
    pub summary: String,
    /// Best-effort list of failing nodes/models (names or dbt node_ids).
    #[serde(default)]
    pub failing_nodes: Vec<String>,
    /// Best-effort list of suggested file paths to inspect/patch next.
    #[serde(default)]
    pub suggested_next_files: Vec<String>,
}

/// Grep dbt log output for error-relevant lines and return them with surrounding context.
///
/// Matches are case-insensitive. Overlapping context windows are merged so no line
/// appears twice.  Output is capped at `max_chars` to keep prompts bounded.
pub fn extract_error_context_lines(log_text: &str, context: usize, max_chars: usize) -> String {
    const PATTERNS: &[&str] = &[
        "error",
        "fail",
        "fatal",
        "exception",
        "traceback",
        "cannot",
        "invalid",
        "not found",
        "no such",
        "unknown",
        "unresolved",
        "ambiguous",
        "mismatch",
    ];

    let lines: Vec<&str> = log_text.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let mut hit = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        let lower = line.to_ascii_lowercase();
        if PATTERNS.iter().any(|p| lower.contains(p)) {
            let start = i.saturating_sub(context);
            let end = (i + context + 1).min(lines.len());
            for slot in &mut hit[start..end] {
                *slot = true;
            }
        }
    }

    let mut out = String::new();
    let mut in_chunk = false;
    for (i, line) in lines.iter().enumerate() {
        if hit[i] {
            if !in_chunk && !out.is_empty() {
                out.push_str("\n  ...\n\n");
            }
            in_chunk = true;
            out.push_str(line);
            out.push('\n');
        } else {
            in_chunk = false;
        }
        if out.len() >= max_chars {
            out.truncate(max_chars);
            out.push('…');
            break;
        }
    }
    out
}

/// Build log excerpts from the dbt validate observation's log streams.
pub fn extract_log_excerpts(obs: &serde_json::Value, context: usize, max_chars: usize) -> String {
    let mut combined = String::new();
    for phase in ["compile", "run_or_build"] {
        for stream in ["stdout", "stderr"] {
            if let Some(text) = obs
                .get("logs")
                .and_then(|v| v.get(phase))
                .and_then(|v| v.get(stream))
                .and_then(|v| v.as_str())
            {
                let cleaned = strip_ansi(text);
                let excerpt = extract_error_context_lines(&cleaned, context, max_chars);
                if !excerpt.trim().is_empty() {
                    if !combined.is_empty() {
                        combined.push_str("\n---\n");
                    }
                    combined.push_str(&format!("[{phase}/{stream}]\n"));
                    combined.push_str(&excerpt);
                }
            }
        }
    }
    combined
}

fn tail_lines(s: &str, max_lines: usize, max_chars: usize) -> String {
    if s.trim().is_empty() {
        return String::new();
    }
    let mut lines: Vec<&str> = s.lines().collect();
    if lines.len() > max_lines {
        lines = lines[lines.len() - max_lines..].to_vec();
    }
    let mut out = lines.join("\n");
    if out.len() > max_chars {
        out.truncate(max_chars);
        out.push('…');
    }
    out
}

/// Summarize a dbt compile/build failure into a terminal-friendly, high-signal summary.
///
/// This is intentionally LLM-driven for accuracy across adapters. Output is a JSON object.
pub async fn summarize_dbt_failure_llm(
    ctx: &react_core::agent::AgentCtx,
    errors: &[String],
    logs: &serde_json::Value,
    max_summary_chars: usize,
) -> Result<DbtFailureSummary, String> {
    let error_brief = compact_brief(errors, 8, 2400);
    let compile_stdout = logs
        .get("compile")
        .and_then(|v| v.get("stdout"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let compile_stderr = logs
        .get("compile")
        .and_then(|v| v.get("stderr"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let run_stdout = logs
        .get("run_or_build")
        .and_then(|v| v.get("stdout"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let run_stderr = logs
        .get("run_or_build")
        .and_then(|v| v.get("stderr"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let user_payload = serde_json::json!({
        "error_brief": error_brief,
        "logs_tail": {
            "compile_stderr_tail": tail_lines(&strip_ansi(compile_stderr), 120, 8000),
            "compile_stdout_tail": tail_lines(&strip_ansi(compile_stdout), 120, 8000),
            "run_or_build_stderr_tail": tail_lines(&strip_ansi(run_stderr), 180, 12000),
            "run_or_build_stdout_tail": tail_lines(&strip_ansi(run_stdout), 180, 12000)
        }
    });

    let schema = react_core::schema_registry::OpenAiStrictSchema::for_type::<DbtFailureSummary>(
        "data_engineer.dbt_failure_summary",
    )
    .map_err(|e| e.to_string())?;

    let sys = concat!(
        "You summarize dbt compilation/build failures for a terminal UI.\n",
        "Goal: accuracy + condensed actionable information.\n",
        "Rules:\n",
        "- Do NOT restate dbt startup banners (Running with dbt=, Registered adapter, Found X models).\n",
        "- Prefer quoting the most actionable dbt/Athena/Trino error line(s).\n",
        "- Include failing model names and file paths when present.\n",
        "- If the error indicates a common root cause (e.g. Athena 'Only one sql statement is allowed', ambiguous column, missing column), say so explicitly.\n",
        "- Keep summary <= max_summary_chars (truncate if needed but preserve the root cause).",
    );
    let msg = react_core::llm::ChatMessage {
        role: react_core::llm::ChatRole::User,
        content: serde_json::json!({
            "max_summary_chars": max_summary_chars.max(200).min(8000),
            "input": user_payload
        })
        .to_string(),
    };
    let messages = vec![
        react_core::llm::ChatMessage {
            role: react_core::llm::ChatRole::System,
            content: sys.to_string(),
        },
        msg,
    ];
    let opts = react_core::llm::LlmCallOptions {
        prompt_id: "data_engineer.dbt_error.summarize",
        expected_format: react_core::llm::LlmExpectedFormat::JsonSchema(schema),
        ..Default::default()
    };
    let mut parsed: DbtFailureSummary = ctx
        .llm_chat_json(&messages, &opts)
        .await
        .map_err(|e| e.to_string())?;
    parsed.summary = strip_ansi(&parsed.summary).trim().to_string();
    if parsed.summary.len() > max_summary_chars {
        parsed.summary.truncate(max_summary_chars);
        parsed.summary.push('…');
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockLlm {
        pub responses: std::sync::Mutex<Vec<String>>,
    }
    impl react_core::llm::LargeLanguageModel for MockLlm {
        fn chat(
            &self,
            _messages: &[react_core::llm::ChatMessage],
            _options: &react_core::llm::LlmCallOptions,
        ) -> Result<String, String> {
            let mut q = self.responses.lock().unwrap();
            if q.is_empty() {
                return Err("no mock responses remaining".to_string());
            }
            Ok(q.remove(0))
        }
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
    }

    #[test]
    fn classify_duplicate_sources() {
        let errs = vec!["Compilation Error\ndbt found two sources with the name \"x\"".to_string()];
        assert_eq!(classify(&errs), DbtErrorClass::DuplicateSources);
    }

    #[test]
    fn classify_workgroup_missing_is_unknown() {
        let errs = vec!["InvalidRequestException: WorkGroup is not found.".to_string()];
        assert_eq!(classify(&errs), DbtErrorClass::Unknown);
    }

    #[test]
    fn classify_missing_source_is_grounding_error() {
        let errs = vec![concat!(
            "Compilation Error: Model 'model.x.y' depends on a source named 'test_raw.raw_products' which was not found"
        )
        .to_string()];
        assert_eq!(classify(&errs), DbtErrorClass::MissingSource);
    }

    #[tokio::test]
    async fn summarize_dbt_failure_llm_parses_and_truncates() {
        use std::sync::Arc;
        let llm: Arc<dyn react_core::llm::LargeLanguageModel> = Arc::new(MockLlm {
            responses: std::sync::Mutex::new(vec![serde_json::json!({
                "summary": "Root cause line 1\nRoot cause line 2",
                "failing_nodes": ["stg_x"],
                "suggested_next_files": ["models/staging/stg_x.sql"]
            })
            .to_string()]),
        });
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(react_module_storage_memory::InMemoryStorageAdapter::default());
        let keyspace: Arc<dyn react_core::keyspace::Keyspace> =
            Arc::new(react_core::keyspace::DefaultKeyspace::new("b".to_string()));
        let scope =
            react_core::scope::RequestScope::parse("t", "w", "p").expect("valid test scope");
        let ctx = react_core::agent::AgentCtxBuilder::new(
            llm,
            storage,
            scope,
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(1)
        .agent_name("test".to_string())
        .build();
        let errors = vec!["Compilation Error: nope".to_string()];
        let logs = serde_json::json!({"compile": {"stdout": "x", "stderr": ""}});
        let out = summarize_dbt_failure_llm(&ctx, &errors, &logs, 10)
            .await
            .unwrap();
        // `.len()` is bytes; ellipsis is multi-byte. Bound by chars.
        assert!(out.summary.chars().count() <= 11); // 10 + ellipsis
        assert_eq!(out.failing_nodes, vec!["stg_x".to_string()]);
    }

    #[test]
    fn extract_error_context_lines_merges_overlapping_chunks() {
        let log = "line 0 ok\nline 1 ok\nline 2 ERROR here\nline 3 ok\nline 4 ok\n\
                   line 5 ok\nline 6 ok\nline 7 FAIL here\nline 8 ok\nline 9 ok\nline 10 ok";
        let result = extract_error_context_lines(log, 2, 10000);
        assert!(result.contains("line 0 ok"), "context before first error");
        assert!(result.contains("ERROR"), "first error");
        assert!(result.contains("FAIL"), "second error");
        let line_count = result.lines().count();
        let unique_lines: std::collections::HashSet<&str> = result.lines().collect();
        assert_eq!(line_count, unique_lines.len(), "no duplicate lines");
    }

    #[test]
    fn extract_error_context_lines_separates_non_adjacent_chunks() {
        let log = "a\nb\nc ERROR\nd\ne\nf\ng\nh\ni\nj\nk\nl FAIL\nm\nn";
        let result = extract_error_context_lines(log, 1, 10000);
        assert!(result.contains("..."), "gap between non-adjacent chunks");
    }

    #[test]
    fn extract_error_context_lines_respects_max_chars() {
        let log = (0..200)
            .map(|i| format!("line {i} ERROR"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = extract_error_context_lines(&log, 0, 500);
        assert!(result.len() <= 510, "within max_chars + ellipsis");
    }

    #[test]
    fn extract_log_excerpts_combines_streams() {
        let obs = serde_json::json!({
            "logs": {
                "compile": { "stdout": "all good\nno issues", "stderr": "" },
                "run_or_build": { "stdout": "line 1\nRuntime Error in model\nline 3", "stderr": "" }
            }
        });
        let result = extract_log_excerpts(&obs, 1, 10000);
        assert!(
            !result.contains("[compile/stdout]"),
            "no error keywords in compile stdout"
        );
        assert!(result.contains("[run_or_build/stdout]"));
        assert!(result.contains("Runtime Error"));
    }
}
