use skippr_lease::DurableError;

const DDL_VERBS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "MERGE",
];

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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn sql_like_prefix_filter() {
        assert!(matches_sql_like("hla_events", None));
        assert!(matches_sql_like("hla_events", Some("hla_%")));
        assert!(!matches_sql_like("other", Some("hla_%")));
        assert!(matches_sql_like("hla_events", Some("hla_events")));
    }

    #[test]
    fn live_wal_scan_sql_parser_is_deleted() {
        let src = include_str!("sql.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(!prod.contains("live_wal_scan"));
        assert!(!prod.contains("iceberg_scan"));
        assert!(!prod.contains("parse_call_args"));
        assert!(!prod.contains("ClassifiedSql"));
    }
}
