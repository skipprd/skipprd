use std::collections::BTreeSet;

/// Sanitize an identifier to a conservative dbt/Athena-friendly form.
///
/// - lowercases
/// - replaces non `[A-Za-z0-9_]` with `_`
/// - collapses repeated `_`
/// - trims leading/trailing `_`
pub fn sanitize_ident(s: &str) -> String {
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

/// Canonical, deterministic staging model name for a dataset identity.
///
/// Intentional 1:1 mapping:
/// `models/staging/stg_<source_schema>_<source_table>.sql`
pub fn canonical_staging_model_name(source_schema: &str, source_table: &str) -> String {
    format!(
        "stg_{}_{}",
        sanitize_ident(source_schema),
        sanitize_ident(source_table)
    )
}

pub fn canonical_staging_rel_path(source_schema: &str, source_table: &str) -> String {
    format!(
        "models/staging/{}.sql",
        canonical_staging_model_name(source_schema, source_table)
    )
}

/// Extract `source('<schema>','<table>')` calls from SQL/Jinja text.
///
/// This is intentionally minimal and robust (not a full SQL/Jinja parser).
/// We accept common formats:
/// - source('a','b')
/// - source('a', 'b')
/// - source(\"a\",\"b\")
/// - source(\"a\", \"b\")
///
/// Returns a de-duplicated list of (schema, table) pairs (both lowercased/trimmed).
pub fn extract_source_calls(sql: &str) -> Vec<(String, String)> {
    let s = sql.to_ascii_lowercase();
    let mut idx = 0usize;
    let mut out: BTreeSet<(String, String)> = BTreeSet::new();
    while let Some(pos) = s[idx..].find("source(") {
        let mut j = idx + pos + "source(".len();

        // Parse first string arg (schema)
        j = skip_ws(&s, j);
        let Some((schema, j2)) = parse_quoted_string(&s, j) else {
            idx = j;
            continue;
        };
        j = skip_ws(&s, j2);
        j = skip_comma(&s, j);
        j = skip_ws(&s, j);

        // Parse second string arg (table)
        let Some((table, j3)) = parse_quoted_string(&s, j) else {
            idx = j;
            continue;
        };

        let schema = schema.trim().to_string();
        let table = table.trim().to_string();
        if !schema.is_empty() && !table.is_empty() {
            out.insert((schema, table));
        }
        idx = j3;
    }
    out.into_iter().collect()
}

pub fn contains_source_call(sql: &str) -> bool {
    !extract_source_calls(sql).is_empty()
}

pub fn contains_ref_call(sql: &str) -> bool {
    sql.to_ascii_lowercase().contains("ref(")
}

pub fn contains_expected_source_call(sql: &str, expected_schema: &str, expected_table: &str) -> bool {
    let es = expected_schema.trim().to_ascii_lowercase();
    let et = expected_table.trim().to_ascii_lowercase();
    extract_source_calls(sql)
        .into_iter()
        .any(|(s, t)| s == es && t == et)
}

fn skip_ws(s: &str, mut i: usize) -> usize {
    let bytes = s.as_bytes();
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn skip_comma(s: &str, mut i: usize) -> usize {
    let bytes = s.as_bytes();
    if i < bytes.len() && bytes[i] == b',' {
        i += 1;
    }
    i
}

fn parse_quoted_string(s: &str, i: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    if i >= bytes.len() {
        return None;
    }
    let q = bytes[i];
    if q != b'\'' && q != b'"' {
        return None;
    }
    let mut j = i + 1;
    while j < bytes.len() && bytes[j] != q {
        j += 1;
    }
    if j >= bytes.len() {
        return None;
    }
    let out = s[i + 1..j].to_string();
    Some((out, j + 1))
}

