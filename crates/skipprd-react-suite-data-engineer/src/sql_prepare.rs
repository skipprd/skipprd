#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedReadOnlySql {
    pub sql: String,
    pub normalized_sql: String,
}

const MUTATING_KEYWORDS: &[&str] = &[
    "alter", "call", "copy", "create", "delete", "drop", "execute", "grant", "insert", "merge",
    "put", "revoke", "set", "truncate", "unload", "update", "use",
];

pub fn prepare_read_only_sql(
    sql: &str,
    default_limit: usize,
) -> Result<PreparedReadOnlySql, String> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err("SQL cannot be empty.".to_string());
    }

    let statement = trim_single_trailing_semicolon(trimmed)?;
    let validation_sql = strip_comments_outside_quotes(statement);
    if contains_semicolon_outside_quotes(&validation_sql) {
        return Err("Only a single read-only SQL statement is allowed.".to_string());
    }

    let tokens = sql_tokens(&validation_sql);
    let Some(first) = tokens.first() else {
        return Err("SQL cannot be empty.".to_string());
    };
    if first != "select" && first != "with" {
        return Err("Only read-only SELECT or WITH queries are supported.".to_string());
    }
    if let Some(keyword) = tokens
        .iter()
        .find(|token| MUTATING_KEYWORDS.contains(&token.as_str()))
    {
        return Err(format!(
            "Only read-only SQL is supported; found disallowed keyword `{keyword}`."
        ));
    }

    let has_limit = tokens.iter().any(|token| token == "limit");
    let mut prepared = statement.to_string();
    if default_limit > 0 && !has_limit {
        prepared.push_str(&format!(" LIMIT {default_limit}"));
    }

    Ok(PreparedReadOnlySql {
        normalized_sql: normalize_sql(&prepared),
        sql: prepared,
    })
}

fn trim_single_trailing_semicolon(sql: &str) -> Result<&str, String> {
    let without_trailing_ws = sql.trim_end();
    let Some(without_semicolon) = without_trailing_ws.strip_suffix(';') else {
        return Ok(without_trailing_ws);
    };
    let trimmed = without_semicolon.trim_end();
    if trimmed.ends_with(';') {
        return Err("Only a single read-only SQL statement is allowed.".to_string());
    }
    Ok(trimmed)
}

fn contains_semicolon_outside_quotes(sql: &str) -> bool {
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut chars = sql.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double_quote => {
                if in_single_quote && chars.peek() == Some(&'\'') {
                    let _ = chars.next();
                } else {
                    in_single_quote = !in_single_quote;
                }
            }
            '"' if !in_single_quote => {
                if in_double_quote && chars.peek() == Some(&'"') {
                    let _ = chars.next();
                } else {
                    in_double_quote = !in_double_quote;
                }
            }
            ';' if !in_single_quote && !in_double_quote => return true,
            _ => {}
        }
    }
    false
}

fn strip_comments_outside_quotes(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut chars = sql.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double_quote => {
                out.push(ch);
                if in_single_quote && chars.peek() == Some(&'\'') {
                    if let Some(escaped) = chars.next() {
                        out.push(escaped);
                    }
                } else {
                    in_single_quote = !in_single_quote;
                }
            }
            '"' if !in_single_quote => {
                out.push(ch);
                if in_double_quote && chars.peek() == Some(&'"') {
                    if let Some(escaped) = chars.next() {
                        out.push(escaped);
                    }
                } else {
                    in_double_quote = !in_double_quote;
                }
            }
            '-' if !in_single_quote && !in_double_quote && chars.peek() == Some(&'-') => {
                let _ = chars.next();
                out.push('\n');
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if !in_single_quote && !in_double_quote && chars.peek() == Some(&'*') => {
                let _ = chars.next();
                out.push(' ');
                let mut prev = '\0';
                for next in chars.by_ref() {
                    if prev == '*' && next == '/' {
                        break;
                    }
                    prev = next;
                }
                out.push(' ');
            }
            _ => out.push(ch),
        }
    }
    out
}

fn sql_tokens(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut chars = sql.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double_quote => {
                if in_single_quote && chars.peek() == Some(&'\'') {
                    let _ = chars.next();
                } else {
                    in_single_quote = !in_single_quote;
                }
                push_token(&mut out, &mut current);
            }
            '"' if !in_single_quote => {
                if in_double_quote && chars.peek() == Some(&'"') {
                    let _ = chars.next();
                } else {
                    in_double_quote = !in_double_quote;
                }
                push_token(&mut out, &mut current);
            }
            _ if in_single_quote || in_double_quote => {}
            _ if ch.is_ascii_alphanumeric() || ch == '_' => current.push(ch),
            _ => push_token(&mut out, &mut current),
        }
    }
    push_token(&mut out, &mut current);
    out
}

fn push_token(out: &mut Vec<String>, current: &mut String) {
    if current.is_empty() {
        return;
    }
    out.push(current.to_ascii_lowercase());
    current.clear();
}

fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_limit_to_select_without_limit() {
        let prepared = prepare_read_only_sql("select * from orders", 50).unwrap();
        assert_eq!(prepared.sql, "select * from orders LIMIT 50");
        assert_eq!(prepared.normalized_sql, "select * from orders limit 50");
    }

    #[test]
    fn preserves_existing_limit() {
        let prepared = prepare_read_only_sql("select * from orders limit 10", 50).unwrap();
        assert_eq!(prepared.sql, "select * from orders limit 10");
    }

    #[test]
    fn trims_single_trailing_semicolon() {
        let prepared = prepare_read_only_sql("select 1;  ", 50).unwrap();
        assert_eq!(prepared.sql, "select 1 LIMIT 50");
    }

    #[test]
    fn rejects_multiple_statements() {
        let err = prepare_read_only_sql("select 1; select 2", 50).unwrap_err();
        assert!(err.contains("single read-only SQL statement"));
    }

    #[test]
    fn rejects_mutating_keywords() {
        let err = prepare_read_only_sql("delete from orders", 50).unwrap_err();
        assert!(err.contains("read-only"));
    }

    #[test]
    fn allows_semicolon_inside_string_literal() {
        let prepared = prepare_read_only_sql("select ';' as value", 50).unwrap();
        assert_eq!(prepared.sql, "select ';' as value LIMIT 50");
    }

    #[test]
    fn ignores_mutating_keywords_inside_string_literals() {
        let prepared = prepare_read_only_sql("select 'delete' as word", 50).unwrap();
        assert_eq!(prepared.sql, "select 'delete' as word LIMIT 50");
    }

    #[test]
    fn allows_leading_line_comment() {
        let sql = "-- skippr-plan-spec-digest: abc\nselect * from orders";
        let prepared = prepare_read_only_sql(sql, 50).unwrap();
        assert_eq!(prepared.sql, format!("{sql} LIMIT 50"));
    }

    #[test]
    fn ignores_mutating_keywords_inside_comments() {
        let sql = "/* drop table orders */\nselect * from orders -- delete everything";
        let prepared = prepare_read_only_sql(sql, 50).unwrap();
        assert_eq!(prepared.sql, format!("{sql} LIMIT 50"));
    }
}
