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
}

