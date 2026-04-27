/// Utilities for parsing engine-native type strings into nested field paths.
///
/// Targets Athena/Glue style types like:
/// - `struct<a:int,b:struct<c:string>>`
/// - `row(a varchar, b row(c double))`
/// - `array<...>`, `map<k,v>` (treated as complex leaves)
///
/// This is best-effort: if parsing fails, callers should fall back to treating the
/// whole column as a single leaf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlattenedField {
    pub path: String,
    pub data_type: String,
    pub expr: Option<String>,
    pub is_complex: bool,
}

pub fn flatten_athena_type(root_col: &str, type_str: &str) -> Vec<FlattenedField> {
    let ty = type_str.trim();
    let mut out: Vec<FlattenedField> = Vec::new();
    let mut stack: Vec<String> = vec![root_col.to_string()];
    parse_type_into(&mut out, &mut stack, ty);
    if out.is_empty() {
        out.push(FlattenedField {
            path: root_col.to_string(),
            data_type: ty.to_string(),
            expr: Some(root_col.to_string()),
            is_complex: is_complex_type(ty),
        });
    }
    out
}

pub fn flatten_type_paths(root_col: &str, type_str: &str) -> Vec<(String, String)> {
    flatten_athena_type(root_col, type_str)
        .into_iter()
        .map(|f| (f.path, f.data_type))
        .collect()
}

fn parse_type_into(out: &mut Vec<FlattenedField>, stack: &mut Vec<String>, ty: &str) {
    let t = ty.trim();
    if t.is_empty() {
        return;
    }
    if let Some(inner) = strip_wrapped(t, "struct<", '>') {
        for (name, child_ty) in parse_struct_fields(inner) {
            stack.push(name);
            parse_type_into(out, stack, &child_ty);
            stack.pop();
        }
        return;
    }
    if let Some(inner) = strip_wrapped(t, "row(", ')') {
        for (name, child_ty) in parse_row_fields(inner) {
            stack.push(name);
            parse_type_into(out, stack, &child_ty);
            stack.pop();
        }
        return;
    }
    if t.to_lowercase().starts_with("array<") || t.to_lowercase().starts_with("map<") {
        push_leaf(out, stack, t, true);
        return;
    }
    push_leaf(out, stack, t, false);
}

fn push_leaf(out: &mut Vec<FlattenedField>, stack: &[String], ty: &str, is_complex: bool) {
    out.push(FlattenedField {
        path: stack.join("."),
        data_type: ty.to_string(),
        expr: build_safe_deref_expr(stack),
        is_complex,
    });
}

fn build_safe_deref_expr(segments: &[String]) -> Option<String> {
    if segments.is_empty() || !segments.iter().all(|s| is_safe_ident(s)) {
        return None;
    }
    Some(segments.join("."))
}

fn is_safe_ident(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap_or('_');
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_complex_type(ty: &str) -> bool {
    let t = ty.trim().to_lowercase();
    t.starts_with("struct<")
        || t.starts_with("row(")
        || t.starts_with("array<")
        || t.starts_with("map<")
}

fn strip_wrapped<'a>(s: &'a str, prefix: &str, closing: char) -> Option<&'a str> {
    let sl = s.trim();
    if !sl.to_lowercase().starts_with(&prefix.to_lowercase()) {
        return None;
    }
    let inner = &sl[prefix.len()..];
    let inner = inner.trim_end();
    if !inner.ends_with(closing) {
        return None;
    }
    Some(inner[..inner.len() - 1].trim())
}

fn split_top_level(s: &str, sep: char) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut angle: i32 = 0;
    let mut paren: i32 = 0;
    for ch in s.chars() {
        match ch {
            '<' => angle += 1,
            '>' => angle -= 1,
            '(' => paren += 1,
            ')' => paren -= 1,
            _ => {}
        }
        if ch == sep && angle == 0 && paren == 0 {
            out.push(cur.trim().to_string());
            cur.clear();
            continue;
        }
        cur.push(ch);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn parse_struct_fields(inner: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for part in split_top_level(inner, ',') {
        let mut angle: i32 = 0;
        let mut paren: i32 = 0;
        let mut split_idx: Option<usize> = None;
        for (i, ch) in part.char_indices() {
            match ch {
                '<' => angle += 1,
                '>' => angle -= 1,
                '(' => paren += 1,
                ')' => paren -= 1,
                ':' if angle == 0 && paren == 0 => {
                    split_idx = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let Some(si) = split_idx else { continue };
        let name = part[..si].trim().to_string();
        let ty = part[si + 1..].trim().to_string();
        if !name.is_empty() && !ty.is_empty() {
            out.push((name, ty));
        }
    }
    out
}

fn parse_row_fields(inner: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for part in split_top_level(inner, ',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut idx: Option<usize> = None;
        let mut angle: i32 = 0;
        let mut paren: i32 = 0;
        for (i, ch) in trimmed.char_indices() {
            match ch {
                '<' => angle += 1,
                '>' => angle -= 1,
                '(' => paren += 1,
                ')' => paren -= 1,
                c if c.is_whitespace() && angle == 0 && paren == 0 => {
                    idx = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let Some(i) = idx else { continue };
        let name = trimmed[..i].trim().to_string();
        let ty = trimmed[i..].trim().to_string();
        if !name.is_empty() && !ty.is_empty() {
            out.push((name, ty));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_struct_nested() {
        let v = flatten_athena_type(
            "context",
            "struct<device:struct<type:string>,session:struct<id:string>>",
        );
        let paths = v.into_iter().map(|f| f.path).collect::<Vec<_>>();
        assert!(paths.contains(&"context.device.type".to_string()));
        assert!(paths.contains(&"context.session.id".to_string()));
    }

    #[test]
    fn flattens_row_nested() {
        let v = flatten_athena_type(
            "context",
            "row(app row(version varchar), session row(id varchar))",
        );
        let paths = v.into_iter().map(|f| f.path).collect::<Vec<_>>();
        assert!(paths.contains(&"context.app.version".to_string()));
        assert!(paths.contains(&"context.session.id".to_string()));
    }

    #[test]
    fn flatten_type_paths_simple() {
        let v = flatten_type_paths("col", "int");
        assert_eq!(v, vec![("col".to_string(), "int".to_string())]);
    }
}
