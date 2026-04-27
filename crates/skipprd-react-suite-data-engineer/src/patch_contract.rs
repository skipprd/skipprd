use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SingleFilePatchArgs {
    pub path: String,
    pub patch_text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct LlmSingleFilePatchResponse {
    pub path: String,
    pub patch_text: String,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct PatchTextNormalization {
    pub patch_text: String,
    pub line_number_headers_rewritten: usize,
    pub git_headers_stripped: bool,
    pub git_metadata_lines_dropped: usize,
}

pub fn single_file_patch_good_example_json() -> &'static str {
    r#"{"op":"patch","path":"models/staging/stg_example.sql","patch_text":"@@ ... @@\n- select old_col from {{ source('raw','events') }}\n+ select new_col from {{ source('raw','events') }}\n"}"#
}

fn normalize_git_header_path(p: &str) -> String {
    let t = p.trim();
    if t == "/dev/null" {
        return t.to_string();
    }
    if let Some(rest) = t.strip_prefix("a/") {
        return rest.to_string();
    }
    if let Some(rest) = t.strip_prefix("b/") {
        return rest.to_string();
    }
    t.to_string()
}

fn is_metadata_line(t: &str) -> bool {
    t.starts_with("diff --git ")
        || t.starts_with("index ")
        || t.starts_with("new file mode ")
        || t.starts_with("deleted file mode ")
        || t.starts_with("similarity index ")
        || t.starts_with("rename from ")
        || t.starts_with("rename to ")
        || t.starts_with("old mode ")
        || t.starts_with("new mode ")
}

fn extract_hunks_from_git_or_unified_patch(
    patch_in: &str,
    expected_rel_path: &str,
) -> Result<(Vec<String>, usize), String> {
    let lines: Vec<&str> = patch_in.lines().collect();
    let mut sections: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let t = lines[i].trim_start();
        if t.starts_with("--- ") {
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }
            if j < lines.len() && lines[j].trim_start().starts_with("+++ ") {
                sections.push((i, j));
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }

    if sections.is_empty() {
        return Err(
            "patch_text must be hunks-only or contain a valid single-file unified diff header pair (---/+++)".to_string(),
        );
    }
    if sections.len() != 1 {
        return Err(
            "patch_text contains multiple file sections; only single-file patches are supported"
                .to_string(),
        );
    }

    let (old_idx, new_idx) = sections[0];
    let old_line = lines[old_idx].trim_start();
    let new_line = lines[new_idx].trim_start();
    let old_path = normalize_git_header_path(old_line.trim_start_matches("--- ").trim());
    let new_path = normalize_git_header_path(new_line.trim_start_matches("+++ ").trim());
    let old_matches = old_path == "/dev/null" || old_path == expected_rel_path;
    let new_matches = new_path == "/dev/null" || new_path == expected_rel_path;
    if !old_matches || !new_matches {
        return Err(format!(
            "patch_text file headers must target expected path '{}'; got old='{}' new='{}'",
            expected_rel_path, old_path, new_path
        ));
    }

    let mut metadata_dropped = 0usize;
    for line in lines.iter().take(old_idx) {
        if is_metadata_line(line.trim_start()) {
            metadata_dropped = metadata_dropped.saturating_add(1);
        }
    }
    for line in lines.iter().take(new_idx).skip(old_idx + 1) {
        if is_metadata_line(line.trim_start()) {
            metadata_dropped = metadata_dropped.saturating_add(1);
        }
    }

    let mut out: Vec<String> = Vec::new();
    let mut in_hunk = false;
    for line in lines.iter().skip(new_idx + 1) {
        let t = line.trim_start();
        if t.starts_with("diff --git ") || t.starts_with("--- ") || t.starts_with("+++ ") {
            break;
        }
        if t.starts_with("@@") {
            in_hunk = true;
            out.push((*line).to_string());
            continue;
        }
        if !in_hunk {
            if t.is_empty() || is_metadata_line(t) {
                if is_metadata_line(t) {
                    metadata_dropped = metadata_dropped.saturating_add(1);
                }
                continue;
            }
            continue;
        }
        out.push((*line).to_string());
    }
    if out.is_empty() || !out.iter().any(|l| l.trim_start().starts_with("@@")) {
        return Err("patch_text did not contain any hunks after header normalization".to_string());
    }
    Ok((out, metadata_dropped))
}

pub fn normalize_hunks_only_patch_text(
    patch_text: &str,
    expected_rel_path: &str,
) -> Result<PatchTextNormalization, String> {
    let patch_in = patch_text.trim();
    if patch_in.is_empty() {
        return Err("patch_text is empty".to_string());
    }

    let has_hunks = patch_in.lines().any(|l| l.trim_start().starts_with("@@"));
    let has_git_headers = patch_in.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("diff --git ")
            || t.starts_with("--- ")
            || t.starts_with("+++ ")
            || is_metadata_line(t)
    });
    let starts_with_hunk = patch_in.trim_start().starts_with("@@");

    let (source_lines, git_headers_stripped, git_metadata_lines_dropped) = if starts_with_hunk
        && !has_git_headers
    {
        (
            patch_in
                .lines()
                .map(|s| s.to_string())
                .collect::<Vec<String>>(),
            false,
            0usize,
        )
    } else if has_hunks && has_git_headers {
        let (out, dropped) = extract_hunks_from_git_or_unified_patch(patch_in, expected_rel_path)?;
        (out, true, dropped)
    } else if has_hunks {
        (
            patch_in
                .lines()
                .map(|s| s.to_string())
                .collect::<Vec<String>>(),
            false,
            0usize,
        )
    } else {
        return Err("patch_text must contain at least one hunk header ('@@')".to_string());
    };

    let mut out: Vec<String> = Vec::new();
    let mut rewritten = 0usize;
    for line in source_lines {
        let t = line.trim_start();
        if t.starts_with("@@ -") {
            rewritten = rewritten.saturating_add(1);
            out.push("@@ ... @@".to_string());
        } else {
            out.push(line);
        }
    }
    if !out
        .first()
        .map(|l| l.trim_start().starts_with("@@"))
        .unwrap_or(false)
    {
        return Err(
            "patch_text must normalize to Cursor/Aider hunks-only and start with '@@'".to_string(),
        );
    }

    Ok(PatchTextNormalization {
        patch_text: out.join("\n"),
        line_number_headers_rewritten: rewritten,
        git_headers_stripped,
        git_metadata_lines_dropped,
    })
}
