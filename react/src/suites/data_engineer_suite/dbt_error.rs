#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbtErrorClass {
    DuplicateSources,
    ProfilesYaml,
    DbtProjectYaml,
    WarehouseConfig,
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

pub fn extract_unresolved_columns(errors: &[String]) -> Vec<String> {
    // Capture patterns like:
    // - Column 'context.session.id' cannot be resolved
    // - Column \"x\" cannot be resolved
    let joined = strip_ansi(&errors.join("\n"));
    let mut out: Vec<String> = Vec::new();
    for line in joined.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        let ll = l.to_lowercase();
        if !ll.contains("cannot be resolved") {
            continue;
        }
        // Single-quoted
        if let Some(start) = l.find("Column '") {
            let rest = &l[start + "Column '".len()..];
            if let Some(end) = rest.find('\'') {
                let col = rest[..end].trim();
                if !col.is_empty() {
                    out.push(col.to_string());
                    continue;
                }
            }
        }
        // Double-quoted
        if let Some(start) = l.find("Column \"") {
            let rest = &l[start + "Column \"".len()..];
            if let Some(end) = rest.find('\"') {
                let col = rest[..end].trim();
                if !col.is_empty() {
                    out.push(col.to_string());
                    continue;
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Extract structured runtime/test failures from dbt `build`/`run` stdout.
///
/// This is intentionally heuristic (no regex deps) but captures the common dbt log pattern:
/// `... FAIL <n> <test_name> ...`.
pub fn extract_runtime_failures_from_logs(logs: &serde_json::Value) -> Vec<serde_json::Value> {
    let stdout = logs
        .get("run_or_build")
        .and_then(|v| v.get("stdout"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    extract_runtime_failures_from_stdout(stdout)
}

fn extract_runtime_failures_from_stdout(stdout: &str) -> Vec<serde_json::Value> {
    let s = strip_ansi(stdout);
    let mut out: Vec<serde_json::Value> = Vec::new();
    for line in s.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        // Typical pattern:
        // `7 of 18 FAIL 2 not_null_stg_customers_customer_created_at ...`
        let Some(idx) = l.find(" FAIL ") else { continue };
        let rest = l[idx + " FAIL ".len()..].trim();
        let mut it = rest.split_whitespace();
        let failures_s = it.next().unwrap_or("");
        let name = it.next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let failures = failures_s.parse::<u64>().ok();
        let (model_hint, column_hint) = parse_test_name_hints(name);
        out.push(serde_json::json!({
            "kind": "test",
            "name": name,
            "failures": failures,
            "model_hint": model_hint,
            "column_hint": column_hint,
            "line": l
        }));
        if out.len() >= 10 {
            break;
        }
    }
    out
}

fn parse_test_name_hints(test_name: &str) -> (Option<String>, Option<String>) {
    // Heuristics for common dbt test naming patterns.
    // Example: not_null_stg_customers_customer_created_at
    //          unique_stg_orders_order_id
    if let Some(rest) = test_name.strip_prefix("not_null_").or_else(|| test_name.strip_prefix("unique_")) {
        let parts: Vec<&str> = rest.split('_').filter(|s| !s.is_empty()).collect();
        if parts.len() >= 3 {
            // Special-case common prefixes like stg_* / dim_* / fct_*.
            let head = parts[0];
            if matches!(head, "stg" | "dim" | "fct") && parts.len() >= 4 {
                let model = format!("{}_{}", parts[0], parts[1]);
                let col = parts[2..].join("_");
                return (Some(model), Some(col));
            }
            let model = parts[0].to_string();
            let col = parts[1..].join("_");
            return (Some(model), Some(col));
        }
    }
    (None, None)
}

pub fn classify(errors: &[String]) -> DbtErrorClass {
    let joined = errors.join("\n");
    let s = strip_ansi(&joined).to_lowercase();

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
    if s.contains("workgroup is not found")
        || (s.contains("datacatalog") && s.contains("was not found"))
        || s.contains("accessdenied")
        || s.contains("expiredtoken")
        || s.contains("signaturedoesnotmatch")
    {
        return DbtErrorClass::WarehouseConfig;
    }
    // Generic SQL failures (compilation/runtime/database execution)
    if s.contains("compilation error")
        || s.contains("runtime error")
        || s.contains("database error")
        || s.contains("failed to execute query")
        || s.contains("invalidrequestexception")
    {
        return DbtErrorClass::SqlFailure;
    }
    if s.contains("sql") {
        return DbtErrorClass::SqlOrModel;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_duplicate_sources() {
        let errs = vec!["Compilation Error\ndbt found two sources with the name \"x\"".to_string()];
        assert_eq!(classify(&errs), DbtErrorClass::DuplicateSources);
    }

    #[test]
    fn classify_workgroup_missing() {
        let errs = vec!["InvalidRequestException: WorkGroup is not found.".to_string()];
        assert_eq!(classify(&errs), DbtErrorClass::WarehouseConfig);
    }

    #[test]
    fn extract_runtime_failures_finds_fail_lines() {
        let stdout = "\
11:19:11  7 of 18 FAIL 2 not_null_stg_customers_customer_created_at .............. [FAIL 2 in 5.89s]\n\
11:19:12  8 of 18 PASS not_null_stg_customers_customer_id ........................ [PASS in 0.10s]\n";
        let logs = serde_json::json!({ "run_or_build": { "stdout": stdout } });
        let rf = extract_runtime_failures_from_logs(&logs);
        assert_eq!(rf.len(), 1);
        assert_eq!(rf[0].get("name").and_then(|v| v.as_str()).unwrap(), "not_null_stg_customers_customer_created_at");
        assert_eq!(rf[0].get("failures").and_then(|v| v.as_u64()).unwrap(), 2);
        assert_eq!(rf[0].get("model_hint").and_then(|v| v.as_str()).unwrap(), "stg_customers");
        assert_eq!(rf[0].get("column_hint").and_then(|v| v.as_str()).unwrap(), "customer_created_at");
    }
}

