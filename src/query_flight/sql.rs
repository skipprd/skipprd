use skippr_lease::{DurableError, PipelineKey};

use crate::cluster::identity::TenantScope;
use crate::query_flight::live_wal::LiveWalScanRequest;

const DDL_VERBS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "MERGE",
];

#[derive(Debug, Clone)]
pub enum ClassifiedSql {
    User,
    LiveWal(LiveWalScanRequest),
    Iceberg(String),
}

pub fn reject_ddl(sql: &str) -> Result<(), DurableError> {
    let masked = mask_sql_strings(&strip_sql_comments(sql)).to_ascii_uppercase();
    for verb in DDL_VERBS {
        if contains_sql_token(&masked, verb) {
            return Err(DurableError::ProtocolMismatch(
                "Flight SQL is read-only".into(),
            ));
        }
    }
    Ok(())
}

pub fn classify_sql(sql: &str, scope: &TenantScope) -> Result<ClassifiedSql, DurableError> {
    match parse_sole_from_tvf(sql) {
        Some(Tvf::LiveWal(args)) => live_wal_from_args(&args, scope)
            .map(ClassifiedSql::LiveWal)
            .ok_or_else(|| {
                DurableError::ProtocolMismatch(
                    "live_wal_scan tenant/workspace does not match session".into(),
                )
            }),
        Some(Tvf::Iceberg(namespace)) => Ok(ClassifiedSql::Iceberg(namespace)),
        None => Ok(ClassifiedSql::User),
    }
}

pub fn parse_live_wal_scan(sql: &str, scope: &TenantScope) -> Option<LiveWalScanRequest> {
    match classify_sql(sql, scope) {
        Ok(ClassifiedSql::LiveWal(request)) => Some(request),
        _ => None,
    }
}

pub fn parse_iceberg_scan(sql: &str) -> Option<String> {
    match parse_sole_from_tvf(sql) {
        Some(Tvf::Iceberg(namespace)) => Some(namespace),
        _ => None,
    }
}

pub fn live_wal_scan_sql(
    pipeline: &PipelineKey,
    namespace: &str,
    exclude_segment_ids: &[String],
) -> String {
    format!(
        "SELECT * FROM live_wal_scan({}, {}, {}, {}, {})",
        sql_string_literal(pipeline.tenant()),
        sql_string_literal(pipeline.workspace()),
        sql_string_literal(pipeline.pipeline()),
        sql_string_literal(namespace),
        sql_string_literal(&exclude_segment_ids.join(";")),
    )
}

pub fn iceberg_scan_sql(namespace: &str) -> String {
    format!(
        "SELECT * FROM iceberg_scan({})",
        sql_string_literal(namespace)
    )
}

pub fn matches_sql_like(name: &str, pattern: Option<&str>) -> bool {
    match pattern {
        None | Some("") => true,
        Some(pattern) if !pattern.contains('%') => name == pattern,
        Some(pattern) => match pattern.strip_suffix('%') {
            Some(prefix) if !prefix.contains('%') => name.starts_with(prefix),
            _ => name == pattern,
        },
    }
}

enum Tvf {
    LiveWal(Vec<String>),
    Iceberg(String),
}

fn parse_sole_from_tvf(sql: &str) -> Option<Tvf> {
    let stripped = strip_sql_comments(sql);
    let masked = mask_sql_strings(&stripped).to_ascii_lowercase();
    let from_start = find_sql_token(&masked, "from")?;
    let table_start = skip_ws(&stripped, from_start + 4);
    let table = stripped[table_start..].trim_start();
    let table_lower = table.to_ascii_lowercase();
    if table_lower.starts_with("live_wal_scan(") {
        let (args, after) = parse_call_args(table, "live_wal_scan")?;
        if !trailing_is_end(after) {
            return None;
        }
        return Some(Tvf::LiveWal(args));
    }
    if table_lower.starts_with("iceberg_scan(") {
        let (args, after) = parse_call_args(table, "iceberg_scan")?;
        if !trailing_is_end(after) || args.len() != 1 {
            return None;
        }
        let namespace = args[0].trim().to_string();
        if namespace.is_empty() {
            return None;
        }
        return Some(Tvf::Iceberg(namespace));
    }
    None
}

fn live_wal_from_args(args: &[String], scope: &TenantScope) -> Option<LiveWalScanRequest> {
    let (pipeline, namespace, exclude_raw) = if args.len() >= 4 {
        if args[0] != scope.tenant || args[1] != scope.workspace {
            return None;
        }
        (
            PipelineKey::new(&args[0], &args[1], &args[2]).ok()?,
            args[3].clone(),
            args.get(4).cloned().unwrap_or_default(),
        )
    } else if args.len() >= 2 {
        (
            PipelineKey::new(&scope.tenant, &scope.workspace, &args[0]).ok()?,
            args[1].clone(),
            args.get(2).cloned().unwrap_or_default(),
        )
    } else {
        return None;
    };
    Some(LiveWalScanRequest {
        pipeline,
        namespace,
        exclude_segment_ids: exclude_raw
            .split(|ch| ch == ',' || ch == ';')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect(),
    })
}

fn parse_call_args<'a>(sql: &'a str, name: &str) -> Option<(Vec<String>, &'a str)> {
    let prefix = format!("{name}(");
    if sql.len() < prefix.len() || !sql[..prefix.len()].eq_ignore_ascii_case(&prefix) {
        return None;
    }
    let rest = &sql[prefix.len()..];
    let end = rest.find(')')?;
    let args = rest[..end]
        .split(',')
        .map(|part| {
            let trimmed = part.trim();
            let unquoted = trimmed
                .trim_matches('\'')
                .trim_matches('"')
                .replace("''", "'");
            unquoted
        })
        .collect();
    Some((args, rest[end + 1..].trim()))
}

fn trailing_is_end(rest: &str) -> bool {
    rest.is_empty() || rest == ";"
}

fn find_sql_token(masked_lower: &str, word: &str) -> Option<usize> {
    let hay = masked_lower.as_bytes();
    let needle = word.as_bytes();
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    for i in 0..=hay.len() - needle.len() {
        if &hay[i..i + needle.len()] == needle
            && token_boundary_before(hay, i)
            && token_boundary_after(hay, i + needle.len())
        {
            return Some(i);
        }
    }
    None
}

fn token_boundary_before(bytes: &[u8], i: usize) -> bool {
    i == 0 || !is_ident_byte(bytes[i - 1])
}

fn token_boundary_after(bytes: &[u8], i: usize) -> bool {
    i >= bytes.len() || !is_ident_byte(bytes[i])
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn contains_sql_token(masked_upper: &str, word: &str) -> bool {
    find_sql_token(
        &masked_upper.to_ascii_lowercase(),
        &word.to_ascii_lowercase(),
    )
    .is_some()
}

fn skip_ws(sql: &str, start: usize) -> usize {
    sql[start..]
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(i, _)| start + i)
        .unwrap_or(sql.len())
}

fn strip_sql_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = i.saturating_add(2);
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn mask_sql_strings(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\'' {
            out.push(' ');
            loop {
                match chars.next() {
                    None => break,
                    Some('\'') if chars.peek() == Some(&'\'') => {
                        chars.next();
                        out.push(' ');
                        out.push(' ');
                    }
                    Some('\'') => {
                        out.push(' ');
                        break;
                    }
                    Some(_) => out.push(' '),
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> TenantScope {
        TenantScope::new("t", "w").unwrap()
    }

    #[test]
    fn ddl_is_rejected() {
        assert!(reject_ddl("INSERT INTO t VALUES (1)").is_err());
        assert!(reject_ddl("select 1").is_ok());
        assert!(reject_ddl("SELECT 'INSERT INTO t' FROM t").is_ok());
        assert!(reject_ddl("-- INSERT INTO t\nSELECT 1").is_ok());
        assert!(reject_ddl("WITH x AS (INSERT INTO t VALUES (1)) SELECT 1").is_err());
        assert!(reject_ddl("SELECT created_at FROM t").is_ok());
    }

    #[test]
    fn live_wal_scan_parses() {
        let req =
            parse_live_wal_scan("SELECT * FROM live_wal_scan('events', 'ns')", &scope()).unwrap();
        assert_eq!(req.pipeline.tenant(), "t");
        assert_eq!(req.pipeline.workspace(), "w");
        assert_eq!(req.pipeline.pipeline(), "events");
        assert_eq!(req.namespace, "ns");
        assert!(req.exclude_segment_ids.is_empty());
        let with_exclude = parse_live_wal_scan(
            "SELECT * FROM live_wal_scan('events', 'ns', 'seg-a;seg-b')",
            &scope(),
        )
        .unwrap();
        assert_eq!(
            with_exclude.exclude_segment_ids,
            vec!["seg-a".to_string(), "seg-b".to_string()]
        );
    }

    #[test]
    fn live_wal_scan_parses_session_scope() {
        let scope = TenantScope::new("acme", "prod").unwrap();
        let req = parse_live_wal_scan(
            "SELECT * FROM live_wal_scan('acme', 'prod', 'events', 'ns')",
            &scope,
        )
        .unwrap();
        assert_eq!(req.pipeline.tenant(), "acme");
        assert_eq!(req.pipeline.workspace(), "prod");
        assert_eq!(req.pipeline.pipeline(), "events");
    }

    #[test]
    fn live_wal_scan_rejects_foreign_tenant() {
        let scope = TenantScope::new("acme", "prod").unwrap();
        assert!(parse_live_wal_scan(
            "SELECT * FROM live_wal_scan('other', 'prod', 'events', 'ns')",
            &scope,
        )
        .is_none());
        let err = classify_sql(
            "SELECT * FROM live_wal_scan('other', 'prod', 'events', 'ns')",
            &scope,
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not match session"));
    }

    #[test]
    fn live_wal_scan_substring_in_user_sql_is_not_intercepted() {
        let sql = "SELECT * FROM events WHERE note = 'live_wal_scan('";
        assert!(matches!(
            classify_sql(sql, &scope()).unwrap(),
            ClassifiedSql::User
        ));
        assert!(parse_live_wal_scan(sql, &scope()).is_none());
    }

    #[test]
    fn iceberg_scan_parses_namespace() {
        assert_eq!(
            parse_iceberg_scan("SELECT * FROM iceberg_scan('hla_events')").as_deref(),
            Some("hla_events")
        );
    }

    #[test]
    fn sql_like_prefix_filter() {
        assert!(matches_sql_like("hla_events", None));
        assert!(matches_sql_like("hla_events", Some("hla_%")));
        assert!(!matches_sql_like("other", Some("hla_%")));
        assert!(matches_sql_like("hla_events", Some("hla_events")));
    }

    #[test]
    fn live_wal_sql_escapes_quotes() {
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let sql = live_wal_scan_sql(&key, "n's", &[]);
        assert!(sql.contains("'n''s'"));
        let req = parse_live_wal_scan(&sql, &scope()).unwrap();
        assert_eq!(req.namespace, "n's");
    }
}
