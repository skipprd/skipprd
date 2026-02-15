use async_trait::async_trait;
use serde::de::Deserializer;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use react_core::agent::AgentCtx;
use react_core::providers::DatasetCatalogProvider;
use react_core::tools::Tool;

use crate::data_engineer::project_fs;

pub struct DbtFilesTool {
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
}

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let out = hasher.finalize();
    hex::encode(out)
}

fn deserialize_opt_nonempty_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let v: Option<String> = Option::deserialize(deserializer)?;
    Ok(v.and_then(|s| {
        let t = s.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    }))
}

fn file_stem(rel_path: &str) -> Option<String> {
    std::path::Path::new(rel_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.trim().is_empty())
}

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
            let Some(pos) = lower[idx..].find(&want) else { break };
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
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\r') {
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
        let from_kw_start =
            from_kw_start.ok_or_else(|| "unable to find FROM for final SELECT in staging SQL".to_string())?;
        let from_list_end =
            from_list_end.ok_or_else(|| "unable to find FROM for final SELECT in staging SQL".to_string())?;

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

fn extract_idents_ending_with_raw(s: &str) -> Vec<String> {
    // Tokenize on non-identifier chars; return tokens ending with _raw.
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        let is_ident = ch.is_ascii_alphanumeric() || ch == '_';
        if is_ident {
            cur.push(ch);
        } else {
            let t = cur.trim();
            if !t.is_empty() && t.to_ascii_lowercase().ends_with("_raw") {
                out.push(t.to_string());
            }
            cur.clear();
        }
    }
    let t = cur.trim();
    if !t.is_empty() && t.to_ascii_lowercase().ends_with("_raw") {
        out.push(t.to_string());
    }
    out.sort();
    out.dedup();
    out
}

fn yaml_collect_model_section<'a>(
    root: &'a serde_yaml::Value,
    model_name: &str,
) -> Vec<&'a serde_yaml::Mapping> {
    let mut out: Vec<&'a serde_yaml::Mapping> = Vec::new();
    let Some(models) = root
        .as_mapping()
        .and_then(|m| m.get(serde_yaml::Value::String("models".to_string())))
        .and_then(|v| v.as_sequence())
    else {
        return out;
    };
    for it in models.iter() {
        let Some(mm) = it.as_mapping() else { continue };
        let name = mm
            .get(serde_yaml::Value::String("name".to_string()))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if name == model_name {
            out.push(mm);
        }
    }
    out
}

fn yaml_collect_column_names(model: &serde_yaml::Mapping) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(cols) = model
        .get(serde_yaml::Value::String("columns".to_string()))
        .and_then(|v| v.as_sequence())
    else {
        return out;
    };
    for c in cols.iter() {
        let Some(cm) = c.as_mapping() else { continue };
        if let Some(n) = cm
            .get(serde_yaml::Value::String("name".to_string()))
            .and_then(|v| v.as_str())
        {
            let t = n.trim();
            if !t.is_empty() {
                out.push(t.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn yaml_collect_where_strings(v: &serde_yaml::Value, out: &mut Vec<String>) {
    match v {
        serde_yaml::Value::Mapping(m) => {
            for (k, val) in m.iter() {
                if k.as_str().map(|s| s == "where").unwrap_or(false) {
                    if let Some(s) = val.as_str() {
                        let t = s.trim();
                        if !t.is_empty() {
                            out.push(t.to_string());
                        }
                    }
                }
                yaml_collect_where_strings(val, out);
            }
        }
        serde_yaml::Value::Sequence(seq) => {
            for it in seq.iter() {
                yaml_collect_where_strings(it, out);
            }
        }
        _ => {}
    }
}

fn disable_contract_enforcement_in_schema_yml_text(yml_text: &str) -> Result<String, String> {
    let mut root: serde_yaml::Value =
        serde_yaml::from_str(yml_text).map_err(|e| format!("invalid YAML: {}", e.to_string()))?;
    let Some(models) = root
        .as_mapping_mut()
        .and_then(|m| m.get_mut(serde_yaml::Value::String("models".to_string())))
        .and_then(|v| v.as_sequence_mut())
    else {
        // Nothing to do.
        return Ok(yml_text.to_string());
    };

    for m in models.iter_mut() {
        let Some(mm) = m.as_mapping_mut() else { continue };
        let cfg = mm
            .entry(serde_yaml::Value::String("config".to_string()))
            .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
        let Some(cfgm) = cfg.as_mapping_mut() else { continue };
        let contract = cfgm
            .entry(serde_yaml::Value::String("contract".to_string()))
            .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
        let Some(cm) = contract.as_mapping_mut() else { continue };
        // Force it off (if present), but keep the structure stable for users who expect it.
        cm.insert(
            serde_yaml::Value::String("enforced".to_string()),
            serde_yaml::Value::Bool(false),
        );
    }

    serde_yaml::to_string(&root)
        .map_err(|e| format!("failed to re-serialize YAML: {}", e.to_string()))
        .map(|s| s.trim_start_matches("---\n").to_string())
}

pub(crate) async fn validate_staging_schema_ymls(
    ctx: &AgentCtx,
    outcomes: &[project_fs::PatchOutcome],
) -> Result<(), String> {
    // Build a rel_path -> new content map so we validate against the content that will be written.
    let mut new_by_rel: HashMap<String, String> = HashMap::new();
    for o in outcomes.iter() {
        new_by_rel.insert(o.rel_path.clone(), o.content.clone());
    }

    for o in outcomes.iter() {
        let rel = o.rel_path.as_str();
        if !(rel.starts_with("models/staging/") && rel.ends_with(".yml")) {
            continue;
        }
        let Some(stem) = file_stem(rel) else { continue };
        let model_name = stem.clone();
        let sql_rel = format!("models/staging/{}.sql", stem);

        let sql_text = if let Some(s) = new_by_rel.get(&sql_rel) {
            s.clone()
        } else {
            // Fall back to existing sibling SQL in storage.
            let key = project_fs::join_storage_key(ctx, &sql_rel);
            let bytes = ctx
                .storage
                .get_bytes(&key)
                .await
                .map_err(|_| format!("cannot validate {}: missing sibling SQL {}", rel, sql_rel))?;
            String::from_utf8_lossy(&bytes).to_string()
        };

        let allowed_cols = extract_final_select_output_columns(&sql_text).map_err(|e| {
            format!(
                "cannot validate {} against {}: {}",
                rel,
                sql_rel,
                e.trim()
            )
        })?;

        let yml_root: serde_yaml::Value = serde_yaml::from_str(&o.content).map_err(|e| {
            format!(
                "invalid YAML in {}: {}",
                rel,
                e.to_string().trim()
            )
        })?;

        let models = yaml_collect_model_section(&yml_root, &model_name);
        if models.is_empty() {
            // If the file doesn't declare the expected model name, skip (best-effort).
            continue;
        }

        fn yaml_model_contract_enforced(model: &serde_yaml::Mapping) -> bool {
            // Expected dbt schema.yml shape:
            // models:
            //   - name: ...
            //     config:
            //       contract:
            //         enforced: true
            let cfg = model
                .get(serde_yaml::Value::String("config".to_string()))
                .and_then(|v| v.as_mapping());
            let enforced = cfg
                .and_then(|m| m.get(serde_yaml::Value::String("contract".to_string())))
                .and_then(|v| v.as_mapping())
                .and_then(|m| m.get(serde_yaml::Value::String("enforced".to_string())));
            enforced.and_then(|v| v.as_bool()).unwrap_or(false)
        }

        fn yaml_collect_column_name_and_type(
            model: &serde_yaml::Mapping,
        ) -> Vec<(String, Option<String>)> {
            let mut out: Vec<(String, Option<String>)> = Vec::new();
            let Some(cols) = model
                .get(serde_yaml::Value::String("columns".to_string()))
                .and_then(|v| v.as_sequence())
            else {
                return out;
            };
            for c in cols.iter() {
                let Some(cm) = c.as_mapping() else { continue };
                let name = cm
                    .get(serde_yaml::Value::String("name".to_string()))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let dt = cm
                    .get(serde_yaml::Value::String("data_type".to_string()))
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                out.push((name, dt));
            }
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        }

        // Contract enforcement is disabled by suite policy.
        // We still validate that schema YAML references only columns produced by sibling SQL
        // (prevents obvious COLUMN_NOT_FOUND and reduces drift), but we do not require full
        // contract completeness or data_type coverage.
        let contract_enforced_any = false;
        let mut missing_cols: BTreeSet<String> = BTreeSet::new();
        let mut missing_data_type: BTreeSet<String> = BTreeSet::new();
        if contract_enforced_any {
            // Consolidate across all declarations for this model name in the file (best-effort).
            let mut declared: HashMap<String, Option<String>> = HashMap::new();
            let mut duplicates: BTreeSet<String> = BTreeSet::new();
            for mm in models.iter() {
                for (name, dt) in yaml_collect_column_name_and_type(mm).into_iter() {
                    if declared.contains_key(&name) {
                        duplicates.insert(name.clone());
                    }
                    declared.insert(name, dt);
                }
            }
            if !duplicates.is_empty() {
                let mut msg = format!(
                    "schema contract has duplicate column entries for staging model '{}'.\nFile: {}\n",
                    model_name, rel
                );
                msg.push_str(&format!(
                    "\nDuplicate columns under models[].columns[]:\n- {}\n",
                    duplicates.into_iter().collect::<Vec<_>>().join("\n- ")
                ));
                msg.push_str("\nFix: de-duplicate YAML columns so each output column appears once.\n");
                return Err(msg);
            }

            for c in allowed_cols.iter() {
                if !declared.contains_key(c) {
                    missing_cols.insert(c.clone());
                }
            }
            for (name, dt) in declared.iter() {
                if allowed_cols.contains(name) && dt.is_none() {
                    missing_data_type.insert(name.clone());
                }
            }
        }

        // (1) Validate declared columns exist in the sibling SQL output.
        let mut unknown_cols: BTreeSet<String> = BTreeSet::new();
        for mm in models.iter() {
            for c in yaml_collect_column_names(mm).into_iter() {
                if !allowed_cols.contains(&c) {
                    unknown_cols.insert(c);
                }
            }
        }

        // (2) Validate any where: predicates that reference *_raw also exist in the sibling output.
        let mut unknown_raw: BTreeSet<String> = BTreeSet::new();
        for mm in models.iter() {
            let mut where_strs: Vec<String> = Vec::new();
            yaml_collect_where_strings(&serde_yaml::Value::Mapping((*mm).clone()), &mut where_strs);
            where_strs.sort();
            where_strs.dedup();
            for ws in where_strs.iter() {
                for tok in extract_idents_ending_with_raw(ws).into_iter() {
                    if !allowed_cols.contains(&tok) {
                        unknown_raw.insert(tok);
                    }
                }
            }
        }

        if !unknown_cols.is_empty() || !unknown_raw.is_empty() {
            let mut msg = format!(
                "schema contract references unknown columns for staging model '{}'.\n\
File: {}\n\
Sibling SQL (defines allowed output columns): {}\n",
                model_name, rel, sql_rel
            );
            if !unknown_cols.is_empty() {
                msg.push_str(&format!(
                    "\nUnknown columns declared under models[].columns[].name:\n- {}\n",
                    unknown_cols.into_iter().collect::<Vec<_>>().join("\n- ")
                ));
            }
            if !unknown_raw.is_empty() {
                msg.push_str(&format!(
                    "\nUnknown *_raw identifiers referenced in where: clauses:\n- {}\n",
                    unknown_raw.into_iter().collect::<Vec<_>>().join("\n- ")
                ));
            }
            msg.push_str("\nFix: either (a) update the staging SQL to actually output these columns, or (b) remove/rename the YAML references to match the staging model output. Do NOT invent new column names.\n");
            return Err(msg);
        }

        if contract_enforced_any && (!missing_cols.is_empty() || !missing_data_type.is_empty()) {
            let mut msg = format!(
                "schema contract is incomplete for contracted staging model '{}'.\n\
File: {}\n\
Sibling SQL (defines required output columns): {}\n",
                model_name, rel, sql_rel
            );
            if !missing_cols.is_empty() {
                msg.push_str(&format!(
                    "\nMissing required columns (must declare every output column when contract is enforced):\n- {}\n",
                    missing_cols.into_iter().collect::<Vec<_>>().join("\n- ")
                ));
            }
            if !missing_data_type.is_empty() {
                msg.push_str(&format!(
                    "\nMissing data_type for columns (required when contract is enforced):\n- {}\n",
                    missing_data_type
                        .into_iter()
                        .collect::<Vec<_>>()
                        .join("\n- ")
                ));
            }
            msg.push_str(
                "\nFix: declare every output column under models[].columns and set data_type for each column.\n",
            );
            return Err(msg);
        }
    }
    Ok(())
}

fn parse_one_or_many<T: DeserializeOwned>(args: &Value, key: &str) -> Result<Vec<T>, String> {
    let Some(v) = args.get(key) else {
        return Ok(Vec::new());
    };
    if v.is_null() {
        return Ok(Vec::new());
    }
    if v.is_array() {
        serde_json::from_value::<Vec<T>>(v.clone())
            .map_err(|e| patch_contract_error(&format!("{} parse error: {}", key, e)))
    } else {
        let one = serde_json::from_value::<T>(v.clone())
            .map_err(|e| patch_contract_error(&format!("{} parse error: {}", key, e)))?;
        Ok(vec![one])
    }
}

fn patch_contract_error(msg: &str) -> String {
    format!(
        "dbt_files op=patch contract violation: {}\n\n{}",
        msg,
        crate::prompts::patch_contract::dbt_files_patch_contract()
    )
}

fn validate_patch_args_shape(args: &Value) -> Result<(), String> {
    fn contains_key_recursive(v: &Value, key: &str) -> bool {
        match v {
            Value::Object(m) => m.contains_key(key) || m.values().any(|vv| contains_key_recursive(vv, key)),
            Value::Array(a) => a.iter().any(|vv| contains_key_recursive(vv, key)),
            _ => false,
        }
    }

    if contains_key_recursive(args, "preview_diff") {
        return Err(patch_contract_error(
            "preview_diff is no longer supported; remove it from the request",
        ));
    }

    // Keep explicit shape errors for common LLM mistakes.
    if let Some(v) = args.get("replace_file") {
        if v.is_string() {
            return Err(patch_contract_error(
                "replace_file must be an object or array (got string)",
            ));
        }
    }
    if let Some(v) = args.get("replace_range") {
        if v.is_string() {
            return Err(patch_contract_error(
                "replace_range must be an object or array (got string)",
            ));
        }
    }
    if let Some(v) = args.get("replace_list") {
        if v.is_string() {
            return Err(patch_contract_error(
                "replace_list must be an object or array (got string)",
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceFileArgs {
    path: String,
    new_text: String,
    #[serde(default, deserialize_with = "deserialize_opt_nonempty_string")]
    expected_sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceRangeArgs {
    path: String,
    start_line: usize,
    end_line: usize,
    new_text: String,
    #[serde(default, deserialize_with = "deserialize_opt_nonempty_string")]
    expected_sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceListEditArgs {
    start_line: usize,
    end_line: usize,
    new_text: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceListArgs {
    path: String,
    edits: Vec<ReplaceListEditArgs>,
    #[serde(default, deserialize_with = "deserialize_opt_nonempty_string")]
    expected_sha256: Option<String>,
}

#[async_trait]
impl Tool for DbtFilesTool {
    fn name(&self) -> &'static str {
        "dbt_files"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let op = args.get("op").and_then(|x| x.as_str()).unwrap_or("get");
        match op {
            "list" => {
                let prefix = args
                    .get("prefix")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .trim();
                let limit = args
                    .get("limit")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(200)
                    .min(2000) as usize;
                project_fs::list_files(ctx, prefix, limit).await
            }
            "get" => {
                let path = args
                    .get("path")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "path required".to_string())?;
                let max_chars =
                    args.get("max_chars").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                project_fs::get_file(ctx, path, max_chars).await
            }
            "get_json" => {
                let path = args
                    .get("path")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "path required".to_string())?;
                let pointer = args.get("pointer").and_then(|x| x.as_str());
                project_fs::get_json(ctx, path, pointer).await
            }
            "manifest_find" => {
                let path = args
                    .get("path")
                    .and_then(|x| x.as_str())
                    .unwrap_or("target/manifest.json");
                let unique_id = args.get("unique_id").and_then(|x| x.as_str());
                let name = args.get("name").and_then(|x| x.as_str());
                let resource_type = args.get("resource_type").and_then(|x| x.as_str());
                let limit = args
                    .get("limit")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(20)
                    .min(200) as usize;
                project_fs::manifest_find(ctx, path, unique_id, name, resource_type, limit).await
            }
            "patch" => {
                validate_patch_args_shape(&args)?;

                // Optional single-file guard: if provided, ensure the patch bundle targets exactly this rel path.
                let want_rel_path = args
                    .get("path")
                    .and_then(|x| x.as_str())
                    .map(project_fs::normalize_rel_path)
                    .transpose()?;

                // Hard-removed: unified diffs are not accepted as input (too flaky for LLMs).
                if args.get("unified_git_style_patch").is_some() {
                    return Err(
                        "dbt_files op=patch no longer accepts unified_git_style_patch. Use exactly one of: replace_file | replace_range | replace_list"
                            .to_string(),
                    );
                }
                let replace_file_ops: Vec<ReplaceFileArgs> =
                    parse_one_or_many(&args, "replace_file")?;
                let replace_range_ops: Vec<ReplaceRangeArgs> =
                    parse_one_or_many(&args, "replace_range")?;
                let replace_list_ops: Vec<ReplaceListArgs> =
                    parse_one_or_many(&args, "replace_list")?;

                let mut provided = 0usize;
                if !replace_file_ops.is_empty() {
                    provided += 1;
                }
                if !replace_range_ops.is_empty() {
                    provided += 1;
                }
                if !replace_list_ops.is_empty() {
                    provided += 1;
                }
                if provided != 1 {
                    return Err(
                        "dbt_files op=patch requires exactly one of: replace_file | replace_range | replace_list"
                            .to_string(),
                    );
                }

                // Deterministic application: compute intended file contents and use FullOverwrite fast-path.
                let mut outcomes: Vec<project_fs::PatchOutcome> = Vec::new();
                let mut seen: HashSet<String> = HashSet::new();
                if !replace_file_ops.is_empty() {
                    for rf in replace_file_ops.into_iter() {
                        let rel = project_fs::normalize_rel_path(&rf.path)?;
                        if !seen.insert(rel.clone()) {
                            return Err(format!("replace_file contains duplicate path: {}", rel));
                        }
                        let key = project_fs::join_storage_key(ctx, &rel);
                        let existing_opt = ctx
                            .storage
                            .get_bytes(&key)
                            .await
                            .ok()
                            .map(|b| String::from_utf8_lossy(&b).to_string());
                        let existed = existing_opt.is_some();
                        let old = existing_opt.unwrap_or_default();
                        let base_sha256 = sha256_hex(&old);
                        if let Some(expected) = rf.expected_sha256.as_deref() {
                            if expected != base_sha256 {
                                return Err(format!(
                                    "expected_sha256 mismatch for {}: expected {}, current {}",
                                    rel, expected, base_sha256
                                ));
                            }
                        }
                        let mut new_text = rf.new_text;
                        if rel.starts_with("models/staging/") && rel.ends_with(".yml") {
                            new_text = disable_contract_enforcement_in_schema_yml_text(&new_text)?;
                        }
                        let out = project_fs::apply_patch(
                            ctx,
                            self.datasets.as_ref(),
                            &rel,
                            &new_text,
                            if existed { Some(base_sha256.as_str()) } else { None },
                            Some(existed),
                            project_fs::PatchApplyKind::FullOverwrite,
                        )
                        .await?;
                        outcomes.push(out);
                    }
                } else if !replace_range_ops.is_empty() {
                    for rr in replace_range_ops.into_iter() {
                        let rel = project_fs::normalize_rel_path(&rr.path)?;
                        if !seen.insert(rel.clone()) {
                            return Err(format!("replace_range contains duplicate path: {}", rel));
                        }
                        let key = project_fs::join_storage_key(ctx, &rel);
                        let existing = ctx
                            .storage
                            .get_bytes(&key)
                            .await
                            .map_err(|_| format!("not found: {}", rel))
                            .map(|b| String::from_utf8_lossy(&b).to_string())?;
                        let base_sha256 = sha256_hex(&existing);
                        if let Some(expected) = rr.expected_sha256.as_deref() {
                            if expected != base_sha256 {
                                return Err(format!(
                                    "expected_sha256 mismatch for {}: expected {}, current {}",
                                    rel, expected, base_sha256
                                ));
                            }
                        }
                        let mut new_text = project_fs::apply_replace_range(
                            &existing,
                            rr.start_line,
                            rr.end_line,
                            &rr.new_text,
                        )?;
                        if rel.starts_with("models/staging/") && rel.ends_with(".yml") {
                            new_text = disable_contract_enforcement_in_schema_yml_text(&new_text)?;
                        }
                        let out = project_fs::apply_patch(
                            ctx,
                            self.datasets.as_ref(),
                            &rel,
                            &new_text,
                            Some(base_sha256.as_str()),
                            Some(true),
                            project_fs::PatchApplyKind::FullOverwrite,
                        )
                        .await?;
                        outcomes.push(out);
                    }
                } else if !replace_list_ops.is_empty() {
                    for rl in replace_list_ops.into_iter() {
                        let rel = project_fs::normalize_rel_path(&rl.path)?;
                        if !seen.insert(rel.clone()) {
                            return Err(format!("replace_list contains duplicate path: {}", rel));
                        }
                        let key = project_fs::join_storage_key(ctx, &rel);
                        let existing = ctx
                            .storage
                            .get_bytes(&key)
                            .await
                            .map_err(|_| format!("not found: {}", rel))
                            .map(|b| String::from_utf8_lossy(&b).to_string())?;
                        let base_sha256 = sha256_hex(&existing);
                        if let Some(expected) = rl.expected_sha256.as_deref() {
                            if expected != base_sha256 {
                                return Err(format!(
                                    "expected_sha256 mismatch for {}: expected {}, current {}",
                                    rel, expected, base_sha256
                                ));
                            }
                        }
                        let edits: Vec<project_fs::ReplaceListEdit> = rl
                            .edits
                            .into_iter()
                            .map(|e| project_fs::ReplaceListEdit {
                                start_line: e.start_line,
                                end_line: e.end_line,
                                new_text: e.new_text,
                            })
                            .collect();
                        let mut new_text = project_fs::apply_replace_list(&existing, &edits)?;
                        if rel.starts_with("models/staging/") && rel.ends_with(".yml") {
                            new_text = disable_contract_enforcement_in_schema_yml_text(&new_text)?;
                        }
                        let out = project_fs::apply_patch(
                            ctx,
                            self.datasets.as_ref(),
                            &rel,
                            &new_text,
                            Some(base_sha256.as_str()),
                            Some(true),
                            project_fs::PatchApplyKind::FullOverwrite,
                        )
                        .await?;
                        outcomes.push(out);
                    }
                } else {
                    return Err("invalid patch request".to_string());
                }
                if outcomes.is_empty() {
                    return Err("patch produced no file changes".to_string());
                }
                let any_mutation = outcomes.iter().any(|o| {
                    o.base_sha256 != o.new_sha256 || (o.lines_added + o.lines_removed) > 0
                });
                if !any_mutation {
                    return Err("patch produced no file changes".to_string());
                }

                if let Some(want) = want_rel_path.as_ref() {
                    let matches: Vec<&project_fs::PatchOutcome> =
                        outcomes.iter().filter(|o| &o.rel_path == want).collect();
                    if matches.len() != 1 || outcomes.len() != 1 {
                        return Err(format!(
                            "patch must target exactly one file '{}' when args.path is provided (got {} file diffs)",
                            want,
                            outcomes.len()
                        ));
                    }
                }

                // Canonical applied patch: based on *postprocessed* file content actually produced by apply_patch.
                outcomes.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
                // Safety: reject staging schema YAML that references columns not produced by
                // the sibling staging SQL output (prevents COLUMN_NOT_FOUND runtime errors).
                validate_staging_schema_ymls(ctx, &outcomes).await?;
                let applied_patch_text = outcomes
                    .iter()
                    .map(|o| o.git_patch.trim_end().to_string())
                    .collect::<Vec<String>>()
                    .join("\n\n");

                let mut results: Vec<Value> = Vec::new();
                let mut written_keys: Vec<String> = Vec::new();
                let mut mutated_any = false;
                for outcome in outcomes.into_iter() {
                    let mutated = outcome.base_sha256 != outcome.new_sha256;
                    mutated_any = mutated_any || mutated;
                    ctx.storage
                        .put_bytes(&outcome.key, outcome.content.as_bytes(), "text/plain")
                        .await?;
                    written_keys.push(outcome.key.clone());
                    results.push(serde_json::json!({
                        "path": outcome.rel_path,
                        "key": outcome.key,
                        "exists": outcome.existed,
                        "mutated": mutated,
                        "base_sha256": outcome.base_sha256,
                        "new_sha256": outcome.new_sha256,
                        "git_patch": outcome.git_patch,
                        "diff": outcome.diff,
                        "lines_added": outcome.lines_added,
                        "lines_removed": outcome.lines_removed
                    }));
                }

                Ok(serde_json::json!({
                    "ok": true,
                    "mutated": mutated_any,
                    "applied_patch_text": applied_patch_text,
                    "written_keys": written_keys,
                    "results": results
                }))
            }
            _ => Err(
                "unsupported op; use 'list', 'get', 'get_json', 'manifest_find', or 'patch'"
                    .to_string(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use react_core::agent::DefaultPolicy;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::NullModel;
    use react_core::scope::RequestScope;
    use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};

    fn minimal_cfg() -> Arc<config::ReactResolvedConfig> {
        Arc::new(config::ReactResolvedConfig {
            server: config::ServerResolved { port: 1 },
            storage: config::StorageResolved {
                bucket: "b".to_string(),
            },
            scope: RequestScope {
                tenant: "t".to_string(),
                workspace: "w".to_string(),
                project_id: "p".to_string(),
            },
            llm: config::LlmResolved::default(),
            providers: config::ProvidersResolved {
                warehouse: config::WarehouseResolved {
                    kind: "athena".to_string(),
                    container: "AwsDataCatalog".to_string(),
                    namespace: "test_raw".to_string(),
                    extras: serde_json::json!({"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"}),
                },
                catalog: config::CatalogResolved {
                    enabled: false,
                    refresh_secs: 60,
                    max_concurrency: 8,
                },
                dbt: config::DbtResolved {
                    enabled: true,
                    profiles_dir: None,
                    target: "athena".to_string(),
                    naming: config::DbtNamingResolved {
                        target_schema: "test".to_string(),
                        silver_suffix: "silver".to_string(),
                        gold_suffix: "warehouse".to_string(),
                    },
                    runner: "host".to_string(),
                    docker_image: None,
                    docker_platform: None,
                    docker_network: None,
                    docker_mount_aws_dir: false,
                },
                vector: config::VectorResolved { enabled: false },
            },
        })
    }

    fn make_ctx(storage: Arc<dyn StorageAdapter>) -> AgentCtx {
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 1,
            thread_id: None,
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(DefaultPolicy),
            llm: Arc::new(NullModel::new()),
            storage,
            scope,
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            runtime: Some(minimal_cfg() as Arc<dyn std::any::Any + Send + Sync>),
        }
    }

    #[tokio::test]
    async fn dbt_files_patch_rejects_patch_text_key() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage);
        let tool = DbtFilesTool { datasets: None };

        let err = tool
            .call(
                serde_json::json!({
                    "op": "patch",
                    "patch_text": "diff --git a/models/x.sql b/models/x.sql\n--- /dev/null\n+++ b/models/x.sql\n@@\n+select 1\n"
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.contains("replace_file"));
    }

    #[tokio::test]
    async fn dbt_files_patch_rejects_unified_git_style_patch_key() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage);
        let tool = DbtFilesTool { datasets: None };

        let err = tool
            .call(
                serde_json::json!({
                    "op": "patch",
                    "unified_git_style_patch": "diff --git a/models/x.sql b/models/x.sql\n--- /dev/null\n+++ b/models/x.sql\n@@\n+select 1\n"
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_lowercase().contains("no longer accepts"));
        assert!(err.contains("replace_file"));
    }

    #[tokio::test]
    async fn dbt_files_patch_replace_file_writes_and_returns_canonical_patch() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage.clone());
        let tool = DbtFilesTool { datasets: None };

        let obs = tool
            .call(
                serde_json::json!({
                    "op": "patch",
                    "replace_file": {
                        "path": "models/x.sql",
                        "new_text": "select 1\n"
                    }
                }),
                &ctx,
            )
            .await
            .expect("patch ok");

        assert_eq!(obs.get("ok").and_then(|v| v.as_bool()), Some(true));
        let applied = obs
            .get("applied_patch_text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(applied.contains("diff --git a/models/x.sql b/models/x.sql"));

        let written = obs
            .get("written_keys")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(written.len(), 1);
        let key = written[0].as_str().unwrap_or("");
        let bytes = storage.get_bytes(key).await.expect("written");
        let content = String::from_utf8_lossy(&bytes).to_string();
        // Hard-cutover portability: do not inject `schema=` into model configs (dbt_project.yml governs schema).
        assert!(!content.contains("config(schema="));
        assert!(content.contains("alias=\"x\""));
        assert!(content.to_ascii_lowercase().contains("select 1"));
    }

    #[tokio::test]
    async fn dbt_files_patch_rejects_preview_diff_anywhere() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage);
        let tool = DbtFilesTool { datasets: None };

        let err = tool
            .call(
                serde_json::json!({
                    "op": "patch",
                    "replace_file": {
                        "path": "models/x.sql",
                        "new_text": "select 1\n",
                        "preview_diff": true
                    }
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_lowercase().contains("contract violation"));
        assert!(err.to_lowercase().contains("preview_diff"));
        assert!(err.to_lowercase().contains("no longer supported"));
    }

    #[tokio::test]
    async fn dbt_files_patch_rejects_replace_file_as_string() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage);
        let tool = DbtFilesTool { datasets: None };

        let err = tool
            .call(
                serde_json::json!({
                    "op": "patch",
                    "replace_file": "models/x.sql"
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_lowercase().contains("contract violation"));
        assert!(err.to_lowercase().contains("replace_file"));
    }

    #[tokio::test]
    async fn dbt_files_patch_rejects_staging_yml_referencing_unknown_columns() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage);
        let tool = DbtFilesTool { datasets: None };

        // Seed sibling staging SQL (explicit select list).
        tool.call(
            serde_json::json!({
                "op": "patch",
                "replace_file": {
                    "path": "models/staging/stg_test_raw_raw_orders.sql",
                    "new_text": "with source as (\n  select\n    '2020-01-01 00:00:00' as placed_at_raw,\n    try_cast('2020-01-01 00:00:00' as timestamp) as placed_at\n  from {{ source('test_raw','raw_orders') }}\n)\n\nselect\n  placed_at_raw,\n  placed_at\nfrom source\n"
                }
            }),
            &ctx,
        )
        .await
        .expect("sql patch ok");

        // Patch YAML that invents created_at_raw / updated_at_raw.
        let err = tool
            .call(
                serde_json::json!({
                    "op": "patch",
                    "replace_file": {
                        "path": "models/staging/stg_test_raw_raw_orders.yml",
                        "new_text": "version: 2\nmodels:\n  - name: stg_test_raw_raw_orders\n    columns:\n      - name: created_at_raw\n        tests:\n          - not_null\n      - name: placed_at\n        tests:\n          - not_null:\n              where: \"updated_at_raw is not null\"\n"
                    }
                }),
                &ctx,
            )
            .await
            .unwrap_err();

        assert!(err.contains("stg_test_raw_raw_orders"));
        assert!(err.contains("models/staging/stg_test_raw_raw_orders.yml"));
        assert!(err.contains("models/staging/stg_test_raw_raw_orders.sql"));
        assert!(err.contains("created_at_raw"));
        assert!(err.contains("updated_at_raw"));
    }

    #[tokio::test]
    async fn dbt_files_patch_allows_staging_yml_when_columns_match_sibling_sql() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage);
        let tool = DbtFilesTool { datasets: None };

        tool.call(
            serde_json::json!({
                "op": "patch",
                "replace_file": {
                    "path": "models/staging/stg_test_raw_raw_orders.sql",
                    "new_text": "with source as (\n  select\n    '2020-01-01 00:00:00' as placed_at_raw,\n    try_cast('2020-01-01 00:00:00' as timestamp) as placed_at\n  from {{ source('test_raw','raw_orders') }}\n)\n\nselect\n  placed_at_raw,\n  placed_at\nfrom source\n"
                }
            }),
            &ctx,
        )
        .await
        .expect("sql patch ok");

        let obs = tool
            .call(
                serde_json::json!({
                    "op": "patch",
                    "replace_file": {
                        "path": "models/staging/stg_test_raw_raw_orders.yml",
                        "new_text": "version: 2\nmodels:\n  - name: stg_test_raw_raw_orders\n    columns:\n      - name: placed_at_raw\n        tests: []\n      - name: placed_at\n        tests:\n          - not_null:\n              where: \"placed_at_raw is not null\"\n"
                    }
                }),
                &ctx,
            )
            .await
            .expect("yml patch ok");

        assert_eq!(obs.get("ok").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn extract_final_select_output_columns_infers_from_cte_when_final_select_star() {
        let sql = r#"
with source as (
  select
    a as a_raw,
    b as b_raw
  from some_table
),
cleaned as (
  select
    a_raw,
    b_raw,
    try_cast(nullif(trim(b_raw), '') as double) as b_num
  from source
)
select *
from cleaned
"#;
        let cols = extract_final_select_output_columns(sql).expect("should infer from cleaned CTE");
        let got = cols.into_iter().collect::<Vec<_>>();
        assert_eq!(got, vec!["a_raw", "b_num", "b_raw"]);
    }

    #[test]
    fn extract_final_select_output_columns_infers_from_cte_when_final_select_alias_star() {
        let sql = r#"
with cleaned as (
  select
    x as x_raw,
    y as y_raw,
    x + y as z
  from t
)
select c.*
from cleaned as c
"#;
        let cols =
            extract_final_select_output_columns(sql).expect("should infer from cleaned CTE via alias.*");
        let got = cols.into_iter().collect::<Vec<_>>();
        assert_eq!(got, vec!["x_raw", "y_raw", "z"]);
    }

    #[test]
    fn extract_final_select_output_columns_still_errors_on_mixed_star_and_explicit() {
        let sql = r#"
with cleaned as (
  select
    x as x_raw,
    y as y_raw
  from t
)
select
  *,
  x_raw
from cleaned
"#;
        let err = extract_final_select_output_columns(sql).unwrap_err();
        assert!(err.to_ascii_lowercase().contains("uses '*'"));
    }

    #[test]
    fn extract_final_select_output_columns_still_errors_on_non_cte_star_from_dotted_target() {
        let sql = r#"
select *
from schema.table
"#;
        let err = extract_final_select_output_columns(sql).unwrap_err();
        assert!(err.to_ascii_lowercase().contains("uses '*'"));
    }

    #[test]
    fn extract_final_select_output_columns_ignores_line_comments_in_select_list() {
        let sql = r#"
with t as (
  select
    'a' as customer_id,
    'x@example.com' as email_raw
)
select
  customer_id,
  -- Email hygiene: trimmed
  email_raw,
  lower(email_raw) as email
from t
"#;
        let cols = extract_final_select_output_columns(sql).expect("should parse select list");
        let got = cols.into_iter().collect::<Vec<_>>();
        assert_eq!(got, vec!["customer_id", "email", "email_raw"]);
    }

    #[test]
    fn extract_final_select_output_columns_ignores_commas_in_line_comments() {
        let sql = r#"
with t as (
  select
    1 as a,
    2 as b
  from some_table
)
select
  a, -- harmless comment, with commas, should not split into phantom columns
  b
from t
"#;
        let cols = extract_final_select_output_columns(sql).expect("should parse select list");
        let got = cols.into_iter().collect::<Vec<_>>();
        assert_eq!(got, vec!["a", "b"]);
    }
}
