use std::collections::BTreeSet;

fn strip_ident_quotes(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2 {
        let bytes = t.as_bytes();
        if (bytes[0] == b'"' && bytes[t.len() - 1] == b'"')
            || (bytes[0] == b'`' && bytes[t.len() - 1] == b'`')
        {
            return t[1..t.len() - 1].to_string();
        }
    }
    t.to_string()
}

fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut depth: i32 = 0;
    let mut in_sq = false;
    let mut in_dq = false;
    let mut prev = '\0';
    for ch in s.chars() {
        if in_sq {
            cur.push(ch);
            if ch == '\'' && prev != '\\' {
                in_sq = false;
            }
            prev = ch;
            continue;
        }
        if in_dq {
            cur.push(ch);
            if ch == '"' && prev != '\\' {
                in_dq = false;
            }
            prev = ch;
            continue;
        }
        match ch {
            '\'' => {
                in_sq = true;
                cur.push(ch);
            }
            '"' => {
                in_dq = true;
                cur.push(ch);
            }
            '(' => {
                depth += 1;
                cur.push(ch);
            }
            ')' => {
                depth = (depth - 1).max(0);
                cur.push(ch);
            }
            ',' if depth == 0 => {
                let t = cur.trim();
                if !t.is_empty() {
                    out.push(t.to_string());
                }
                cur.clear();
            }
            _ => cur.push(ch),
        }
        prev = ch;
    }
    let t = cur.trim();
    if !t.is_empty() {
        out.push(t.to_string());
    }
    out
}

pub(crate) fn extract_final_select_output_columns(sql: &str) -> Result<BTreeSet<String>, String> {
    fn is_boundary(prev: Option<char>) -> bool {
        match prev {
            None => true,
            Some(c) => !(c.is_ascii_alphanumeric() || c == '_'),
        }
    }

    fn parse_quoted_token(s: &str) -> Option<(String, usize)> {
        let bytes = s.as_bytes();
        if bytes.is_empty() {
            return None;
        }
        let q = bytes[0];
        if q != b'"' && q != b'`' {
            return None;
        }
        for i in 1..bytes.len() {
            if bytes[i] == q && bytes[i.saturating_sub(1)] != b'\\' {
                let t = &s[..=i];
                return Some((t.to_string(), i + 1));
            }
        }
        None
    }

    fn parse_simple_token(s: &str) -> Option<(String, usize)> {
        let mut end = 0usize;
        for (i, ch) in s.char_indices() {
            if !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '.') {
                break;
            }
            end = i + ch.len_utf8();
        }
        if end == 0 {
            None
        } else {
            Some((s[..end].to_string(), end))
        }
    }

    fn parse_from_target_and_alias(from_rest: &str) -> Option<(String, Option<String>)> {
        let mut s = from_rest.trim_start();
        if s.is_empty() {
            return None;
        }
        if s.starts_with('(') {
            // FROM (subquery ...) is ambiguous; don't guess.
            return None;
        }
        let (tok1, n1) = parse_quoted_token(s).or_else(|| parse_simple_token(s))?;
        s = &s[n1..];
        let target = strip_ident_quotes(tok1.trim());
        if target.is_empty() {
            return None;
        }
        // Optional alias.
        let mut s2 = s.trim_start();
        if s2.is_empty() {
            return Some((target, None));
        }
        // Handle optional AS.
        if s2.to_ascii_lowercase().starts_with("as ") {
            s2 = s2[3..].trim_start();
        }
        let (tok2, _n2) = match parse_quoted_token(s2).or_else(|| parse_simple_token(s2)) {
            Some(v) => v,
            None => return Some((target, None)),
        };
        let alias = strip_ident_quotes(tok2.trim());
        let al = alias.to_ascii_lowercase();
        // Don't treat SQL keywords as aliases.
        let is_keyword = matches!(
            al.as_str(),
            "where"
                | "group"
                | "order"
                | "limit"
                | "join"
                | "inner"
                | "left"
                | "right"
                | "full"
                | "cross"
                | "union"
                | "on"
        );
        if alias.is_empty() || is_keyword {
            Some((target, None))
        } else {
            Some((target, Some(alias)))
        }
    }

    fn find_cte_body(sql: &str, cte_name: &str) -> Option<String> {
        let lower = sql.to_ascii_lowercase();
        let want = cte_name.to_ascii_lowercase();
        if want.is_empty() {
            return None;
        }

        let mut idx = 0usize;
        while idx < lower.len() {
            let Some(pos) = lower[idx..].find(&want) else {
                break;
            };
            let start = idx + pos;
            let prev = lower[..start].chars().rev().next();
            if !is_boundary(prev) {
                idx = start + want.len();
                continue;
            }
            let mut j = start + want.len();
            // Skip whitespace.
            while j < lower.len() && lower.as_bytes()[j].is_ascii_whitespace() {
                j += 1;
            }
            if !lower[j..].starts_with("as") {
                idx = start + want.len();
                continue;
            }
            j += 2;
            while j < lower.len() && lower.as_bytes()[j].is_ascii_whitespace() {
                j += 1;
            }
            if j >= lower.len() || lower.as_bytes()[j] != b'(' {
                idx = start + want.len();
                continue;
            }
            let open = j;

            // Find matching ')', respecting nested parens and quoted strings.
            let mut depth: i32 = 0;
            let mut in_sq = false;
            let mut in_dq = false;
            let mut prev_ch = '\0';
            for (off, ch) in sql[open..].char_indices() {
                if in_sq {
                    if ch == '\'' && prev_ch != '\\' {
                        in_sq = false;
                    }
                    prev_ch = ch;
                    continue;
                }
                if in_dq {
                    if ch == '"' && prev_ch != '\\' {
                        in_dq = false;
                    }
                    prev_ch = ch;
                    continue;
                }
                match ch {
                    '\'' => in_sq = true,
                    '"' => in_dq = true,
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            let close = open + off;
                            if close > open + 1 {
                                return Some(sql[open + 1..close].to_string());
                            } else {
                                return Some(String::new());
                            }
                        }
                    }
                    _ => {}
                }
                prev_ch = ch;
            }
            return None;
        }
        None
    }

    fn inner(sql: &str, depth: usize) -> Result<BTreeSet<String>, String> {
        if depth > 3 {
            return Err("unable to resolve final SELECT '*' chain (too deep)".to_string());
        }

        // Conservative heuristic: staging models written by this suite typically end with:
        //   select
        //     a,
        //     expr as b
        //   from ...
        //
        // We parse the final SELECT list and return a set of output column names.
        let lower = sql.to_ascii_lowercase();
        let mut last_select: Option<usize> = None;
        let mut idx = 0usize;
        while let Some(pos) = lower[idx..].find("select") {
            last_select = Some(idx + pos);
            idx = idx + pos + "select".len();
        }
        let sel = last_select.ok_or_else(|| "unable to find SELECT in staging SQL".to_string())?;
        let after_sel = sel + "select".len();

        // Prefer a line-starting FROM (allowing indentation) to avoid matching 'from' inside expressions.
        let mut from_kw_start: Option<usize> = None;
        let mut from_list_end: Option<usize> = None; // exclusive index into sql
        let bytes = lower.as_bytes();
        let mut i = after_sel;
        while i < bytes.len() {
            if bytes[i] == b'\n' {
                let mut j = i + 1;
                // Skip indentation.
                while j < bytes.len()
                    && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\r')
                {
                    j += 1;
                }
                if j + 4 <= bytes.len() && &lower[j..j + 4] == "from" {
                    from_kw_start = Some(j);
                    // list ends at the newline (or preceding \r).
                    let mut end = i;
                    if end > after_sel && bytes[end.saturating_sub(1)] == b'\r' {
                        end = end.saturating_sub(1);
                    }
                    from_list_end = Some(end);
                    break;
                }
            }
            i += 1;
        }
        let from_kw_start = from_kw_start
            .ok_or_else(|| "unable to find FROM for final SELECT in staging SQL".to_string())?;
        let from_list_end = from_list_end
            .ok_or_else(|| "unable to find FROM for final SELECT in staging SQL".to_string())?;

        fn strip_sql_line_comments_outside_quotes_full_text(s: &str) -> String {
            // Remove `-- ...` line comments (outside quotes/backticks) from the full select-list
            // slice *before* comma splitting. This prevents commas in comments from producing
            // phantom select items.
            let mut out = String::new();
            for line in s.lines() {
                let mut in_sq = false;
                let mut in_dq = false;
                let mut in_bt = false;
                let mut prev = '\0';
                let mut kept = String::new();
                let mut chars = line.chars().peekable();
                while let Some(ch) = chars.next() {
                    match ch {
                        '\'' if !in_dq && !in_bt && prev != '\\' => in_sq = !in_sq,
                        '"' if !in_sq && !in_bt && prev != '\\' => in_dq = !in_dq,
                        '`' if !in_sq && !in_dq => in_bt = !in_bt,
                        '-' if !in_sq && !in_dq && !in_bt => {
                            if let Some('-') = chars.peek().copied() {
                                break; // comment start
                            }
                            kept.push(ch);
                        }
                        _ => kept.push(ch),
                    }
                    prev = ch;
                }
                out.push_str(&kept);
                out.push('\n');
            }
            out
        }

        fn strip_sql_line_comments_outside_quotes_one_line(s: &str) -> String {
            // Same logic as above, but for a single expression string.
            strip_sql_line_comments_outside_quotes_full_text(s).replace('\n', " ")
        }

        fn is_conservative_identifier(name: &str) -> bool {
            let mut it = name.chars();
            let Some(first) = it.next() else { return false };
            if !(first.is_ascii_alphabetic() || first == '_') {
                return false;
            }
            for ch in it {
                if !(ch.is_ascii_alphanumeric() || ch == '_') {
                    return false;
                }
            }
            true
        }

        let list = &sql[after_sel..from_list_end];
        let list_sans_comments = strip_sql_line_comments_outside_quotes_full_text(list);
        let items = split_top_level_commas(&list_sans_comments);
        if items.is_empty() {
            return Err("final SELECT list appears empty".to_string());
        }

        // If the final SELECT is a wildcard, attempt to infer allowed columns from the referenced CTE.
        // We only allow safe patterns: `select * from <cte>` or `select <alias>.* from <cte> <alias>`.
        let trimmed_items: Vec<String> = items.iter().map(|s| s.trim().to_string()).collect();
        let wildcard_item = if trimmed_items.len() == 1 {
            let one = trimmed_items[0].trim();
            if one == "*" || one.ends_with(".*") {
                Some(one.to_string())
            } else {
                None
            }
        } else {
            None
        };
        if let Some(wc) = wildcard_item {
            let from_kw_end = from_kw_start + "from".len();
            let from_rest = &sql[from_kw_end..];
            let (target, alias) = parse_from_target_and_alias(from_rest).ok_or_else(|| {
                "staging SQL uses '*' in final SELECT; cannot safely validate schema YAML (use an explicit select list)"
                    .to_string()
            })?;
            // Only infer for CTE-like single identifiers (no dotted paths).
            if target.contains('.') {
                return Err(
                    "staging SQL uses '*' in final SELECT; cannot safely validate schema YAML (use an explicit select list)"
                        .to_string(),
                );
            }
            if wc != "*" {
                // Expect <prefix>.* and ensure prefix matches FROM alias (or target when no alias).
                let prefix = wc.trim_end_matches(".*").trim();
                let prefix = strip_ident_quotes(prefix);
                let ok_prefix = if let Some(a) = alias.as_ref() {
                    prefix == *a
                } else {
                    prefix == target
                };
                if !ok_prefix {
                    return Err(
                        "staging SQL uses '*' in final SELECT; cannot safely validate schema YAML (use an explicit select list)"
                            .to_string(),
                    );
                }
            }
            let body = find_cte_body(sql, &target).ok_or_else(|| {
                "staging SQL uses '*' in final SELECT; cannot safely validate schema YAML (use an explicit select list)"
                    .to_string()
            })?;
            // Recurse into the CTE body and extract its final explicit projection.
            return inner(&body, depth + 1);
        }

        let mut out: BTreeSet<String> = BTreeSet::new();

        for it in items {
            let t = strip_sql_line_comments_outside_quotes_one_line(it.trim());
            let t = t.trim();
            if t.is_empty() {
                continue;
            }
            if t == "*" || t.ends_with(".*") {
                return Err(
                    "staging SQL uses '*' in final SELECT; cannot safely validate schema YAML (use an explicit select list)"
                        .to_string(),
                );
            }
            let tl = t.to_ascii_lowercase();
            if let Some(as_pos) = tl.rfind(" as ") {
                let alias = strip_ident_quotes(&t[as_pos + 4..]);
                let a = alias.trim();
                if !a.is_empty() && is_conservative_identifier(a) {
                    out.insert(a.to_string());
                }
                continue;
            }
            // Bare identifier or quoted identifier.
            let ident = strip_ident_quotes(t);
            let name = ident.split('.').last().unwrap_or("").trim().to_string();
            if !name.is_empty() && is_conservative_identifier(&name) {
                out.insert(name);
            }
        }
        if out.is_empty() {
            return Err("unable to extract any output columns from final SELECT".to_string());
        }
        Ok(out)
    }

    inner(sql, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_columns_with_case_end_as() {
        let sql = r#"{{ config(alias="stg_test_raw_raw_orders") }}

with source_data as (
    select
        customer_id,
        placed_at,
        total_amount,
        order_id,
        order_status
    from {{ source("test_raw", "raw_orders") }}
),
normalized as (
    select
        customer_id as customer_id_raw,
        placed_at as placed_at_raw,
        total_amount as total_amount_raw,
        order_id as order_id_raw,
        order_status as order_status_raw,
        nullif(trim(customer_id), '') as customer_id,
        nullif(trim(placed_at), '') as placed_at_text,
        nullif(trim(total_amount), '') as total_amount_text,
        nullif(trim(order_id), '') as order_id,
        lower(nullif(trim(order_status), '')) as order_status,
        nullif(trim(order_status), '') as order_status_trimmed,
        case
            when customer_id is not null and trim(customer_id) <> customer_id then true
            when placed_at is not null and trim(placed_at) <> placed_at then true
            when total_amount is not null and trim(total_amount) <> total_amount then true
            when order_id is not null and trim(order_id) <> order_id then true
            when order_status is not null and trim(order_status) <> order_status then true
            else false
        end as had_whitespace_issue
    from source_data
),
typed as (
    select
        customer_id_raw,
        placed_at_raw,
        total_amount_raw,
        order_id_raw,
        order_status_raw,
        customer_id,
        try_cast(placed_at_text as timestamp) as placed_at,
        try_cast(total_amount_text as decimal(18,2)) as total_amount,
        order_id,
        order_status,
        placed_at_text,
        total_amount_text,
        order_status_trimmed,
        had_whitespace_issue
    from normalized
)
select
    customer_id_raw,
    placed_at_raw,
    total_amount_raw,
    order_id_raw,
    order_status_raw,
    customer_id,
    placed_at,
    total_amount,
    order_id,
    order_status,
    case
        when order_status_trimmed is not null and lower(order_status_trimmed) <> order_status_trimmed then true
        else false
    end as order_status_case_normalized,
    case
        when placed_at_text is not null and placed_at is null then true
        else false
    end as placed_at_parse_failed,
    case
        when total_amount_text is not null and total_amount is null then true
        else false
    end as total_amount_cast_failed,
    case
        when total_amount is not null and total_amount < 0 then true
        else false
    end as total_amount_negative,
    had_whitespace_issue
from typed"#;
        let cols = extract_final_select_output_columns(sql).unwrap();
        assert!(
            cols.contains("order_status_case_normalized"),
            "should find order_status_case_normalized; got: {:?}",
            cols
        );
        assert!(cols.contains("placed_at_parse_failed"));
        assert!(cols.contains("total_amount_cast_failed"));
        assert!(cols.contains("total_amount_negative"));
        assert!(cols.contains("had_whitespace_issue"));
        assert!(cols.contains("customer_id_raw"));
    }
}
