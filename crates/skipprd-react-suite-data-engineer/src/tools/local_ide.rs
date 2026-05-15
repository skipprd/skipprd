use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use react_core::agent::AgentCtx;
use react_core::tools::Tool;

use crate::patch_contract::normalize_hunks_only_patch_text;

const DEFAULT_MAX_CHARS: usize = 20_000;
const HARD_MAX_CHARS: usize = 60_000;
const DEFAULT_LIMIT: usize = 100;
const HARD_LIMIT: usize = 500;

pub struct LocalIdeTool {
    pub allow_patch: bool,
}

#[derive(Debug, Deserialize)]
struct LocalIdeArgs {
    op: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    pattern: Option<String>,
    #[serde(default)]
    patch_text: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    max_chars: Option<usize>,
    #[serde(default)]
    lines: Option<usize>,
}

#[async_trait]
impl Tool for LocalIdeTool {
    fn name(&self) -> &'static str {
        "local_ide"
    }

    async fn call(&self, args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
        let parsed: LocalIdeArgs =
            serde_json::from_value(args).map_err(|e| format!("local_ide args error: {e}"))?;
        match parsed.op.as_str() {
            "list" => op_list(&parsed),
            "read" => op_read(&parsed),
            "grep" => op_grep(&parsed),
            "head" => op_head_tail(&parsed, true),
            "tail" => op_head_tail(&parsed, false),
            "patch" if self.allow_patch => op_patch(&parsed),
            "patch" => Err(
                "local_ide patch is not available in this mode; switch to agent mode for local file mutations"
                    .to_string(),
            ),
            other => Err(format!(
                "unsupported local_ide op '{other}' (expected list|read|grep|head|tail{})",
                if self.allow_patch { "|patch" } else { "" }
            )),
        }
    }
}

fn limit(args: &LocalIdeArgs) -> usize {
    args.limit.unwrap_or(DEFAULT_LIMIT).min(HARD_LIMIT).max(1)
}

fn max_chars(args: &LocalIdeArgs) -> usize {
    args.max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .min(HARD_MAX_CHARS)
        .max(1)
}

fn lines(args: &LocalIdeArgs) -> usize {
    args.lines.or(args.limit).unwrap_or(40).min(200).max(1)
}

fn path_arg(args: &LocalIdeArgs) -> Result<PathBuf, String> {
    let raw = args.path.as_deref().unwrap_or(".");
    let path = if let Some(rest) = raw.strip_prefix("file://") {
        PathBuf::from(rest)
    } else {
        PathBuf::from(raw)
    };
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|e| format!("failed to resolve current dir: {e}"))
    }
}

fn display_path(path: &Path) -> String {
    let abs = path.to_string_lossy().replace('\\', "/");
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(rel) = path.strip_prefix(cwd) {
            return rel.to_string_lossy().replace('\\', "/");
        }
    }
    abs
}

fn hash_text(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn bounded_text(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }
    (text.chars().take(max_chars).collect(), true)
}

fn op_list(args: &LocalIdeArgs) -> Result<Value, String> {
    let path = path_arg(args)?;
    let meta = fs::metadata(&path)
        .map_err(|e| format!("local_ide list failed for {}: {e}", path.display()))?;
    if !meta.is_dir() {
        return Err(format!(
            "local_ide list path is not a directory: {}",
            path.display()
        ));
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(&path)
        .map_err(|e| format!("local_ide list read_dir failed for {}: {e}", path.display()))?
    {
        let entry = entry.map_err(|e| format!("local_ide list entry error: {e}"))?;
        let p = entry.path();
        let m = entry
            .metadata()
            .map_err(|e| format!("local_ide list metadata failed for {}: {e}", p.display()))?;
        entries.push(serde_json::json!({
            "name": entry.file_name().to_string_lossy(),
            "path": display_path(&p),
            "kind": if m.is_dir() { "dir" } else if m.is_file() { "file" } else { "other" },
            "size": if m.is_file() { Some(m.len()) } else { None },
        }));
    }
    entries.sort_by(|a, b| {
        a.get("path")
            .and_then(|v| v.as_str())
            .cmp(&b.get("path").and_then(|v| v.as_str()))
    });
    entries.truncate(limit(args));
    Ok(serde_json::json!({"ok": true, "path": display_path(&path), "entries": entries}))
}

fn op_read(args: &LocalIdeArgs) -> Result<Value, String> {
    let path = path_arg(args)?;
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("local_ide read failed for {}: {e}", path.display()))?;
    let (content, truncated) = bounded_text(&text, max_chars(args));
    Ok(serde_json::json!({
        "ok": true,
        "path": display_path(&path),
        "sha256": hash_text(&text),
        "truncated": truncated,
        "content": content,
    }))
}

fn should_skip_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|name| {
            matches!(
                name,
                ".git" | "node_modules" | "target" | "dist" | "build" | ".next" | ".vite"
            )
        })
        .unwrap_or(false)
}

fn op_grep(args: &LocalIdeArgs) -> Result<Value, String> {
    let pattern = args
        .pattern
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "local_ide grep requires pattern".to_string())?;
    let path = path_arg(args)?;
    let cap = limit(args);
    let mut matches = Vec::new();
    let paths: Vec<PathBuf> = if path.is_file() {
        vec![path.clone()]
    } else if path.is_dir() {
        WalkDir::new(&path)
            .into_iter()
            .filter_entry(|e| !should_skip_dir(e.path()))
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
            .collect()
    } else {
        return Err(format!(
            "local_ide grep path is not readable: {}",
            path.display()
        ));
    };
    for file in paths {
        if matches.len() >= cap {
            break;
        }
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        for (idx, line) in text.lines().enumerate() {
            if line.contains(pattern) {
                let (snippet, truncated) = bounded_text(line, 500);
                matches.push(serde_json::json!({
                    "path": display_path(&file),
                    "line": idx + 1,
                    "text": snippet,
                    "truncated": truncated,
                }));
                if matches.len() >= cap {
                    break;
                }
            }
        }
    }
    Ok(
        serde_json::json!({"ok": true, "pattern": pattern, "path": display_path(&path), "matches": matches, "truncated": matches.len() >= cap}),
    )
}

fn op_head_tail(args: &LocalIdeArgs, head: bool) -> Result<Value, String> {
    let path = path_arg(args)?;
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("local_ide read failed for {}: {e}", path.display()))?;
    let n = lines(args);
    let all: Vec<&str> = text.lines().collect();
    let selected: Vec<&str> = if head {
        all.iter().take(n).copied().collect()
    } else {
        all.iter()
            .skip(all.len().saturating_sub(n))
            .copied()
            .collect()
    };
    let joined = selected.join("\n");
    let (content, truncated) = bounded_text(&joined, max_chars(args));
    Ok(serde_json::json!({
        "ok": true,
        "op": if head { "head" } else { "tail" },
        "path": display_path(&path),
        "lines": n,
        "truncated": truncated,
        "content": content,
    }))
}

fn op_patch(args: &LocalIdeArgs) -> Result<Value, String> {
    let path = path_arg(args)?;
    let patch_text = args
        .patch_text
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "local_ide patch requires patch_text".to_string())?;
    let existed = path.exists();
    let old = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(format!(
                "local_ide patch read failed for {}: {e}",
                path.display()
            ))
        }
    };
    let shown = display_path(&path);
    let normalized = normalize_hunks_only_patch_text(patch_text, &shown)
        .map_err(|e| format!("local_ide patch contract violation: {e}"))?;
    let new = apply_cursor_hunks(&old, &normalized.patch_text)?;
    let before_sha256 = hash_text(&old);
    let after_sha256 = hash_text(&new);
    let no_op = before_sha256 == after_sha256;
    if !no_op {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "local_ide patch failed to create parent {}: {e}",
                    parent.display()
                )
            })?;
        }
        fs::write(&path, new.as_bytes())
            .map_err(|e| format!("local_ide patch write failed for {}: {e}", path.display()))?;
    }
    let lines_added = normalized
        .patch_text
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count();
    let lines_removed = normalized
        .patch_text
        .lines()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .count();
    let applied_patch_text = if no_op {
        String::new()
    } else {
        create_local_git_patch_text(&old, &new, &shown, existed)
    };
    Ok(serde_json::json!({
        "ok": true,
        "path": shown,
        "absolute_path": path.to_string_lossy().replace('\\', "/"),
        "before_sha256": before_sha256,
        "after_sha256": after_sha256,
        "before_content": old,
        "after_content": new,
        "applied_patch_text": applied_patch_text,
        "normalized_patch_text": normalized.patch_text,
        "lines_added": lines_added,
        "lines_removed": lines_removed,
        "no_op": no_op,
        "normalization": {
            "line_number_headers_rewritten": normalized.line_number_headers_rewritten,
            "git_headers_stripped": normalized.git_headers_stripped,
            "git_metadata_lines_dropped": normalized.git_metadata_lines_dropped,
        }
    }))
}

fn apply_cursor_hunks(old: &str, patch_text: &str) -> Result<String, String> {
    let mut current: Vec<String> = old.lines().map(|line| line.to_string()).collect();
    let had_trailing_newline = old.ends_with('\n');
    let mut hunk: Vec<&str> = Vec::new();
    for line in patch_text.lines() {
        if line.trim_start().starts_with("@@") {
            if !hunk.is_empty() {
                apply_one_hunk(&mut current, &hunk)?;
                hunk.clear();
            }
            continue;
        }
        if line.starts_with(' ') || line.starts_with('-') || line.starts_with('+') {
            hunk.push(line);
        } else if !line.trim().is_empty() {
            return Err(format!("local_ide patch invalid hunk line: {line}"));
        }
    }
    if !hunk.is_empty() {
        apply_one_hunk(&mut current, &hunk)?;
    }
    let mut out = current.join("\n");
    if had_trailing_newline || old.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

fn create_local_git_patch_text(
    old: &str,
    new: &str,
    display_path: &str,
    existed: bool,
) -> String {
    let rel = display_path.trim_start_matches("./").replace('\\', "/");
    let base = diffy::create_patch(old, new).to_string();
    if base.lines().take(2).count() < 2 {
        return String::new();
    }

    let header_old = if existed {
        format!("--- a/{rel}")
    } else {
        "--- /dev/null".to_string()
    };
    let header_new = format!("+++ b/{rel}");
    let mut out: Vec<String> = Vec::new();
    out.push(format!("diff --git a/{0} b/{0}", rel));
    if !existed {
        out.push("new file mode 100644".to_string());
    }
    out.push(header_old);
    out.push(header_new);
    for l in base.lines().skip(2) {
        out.push(l.to_string());
    }
    out.join("\n")
}

fn apply_one_hunk(current: &mut Vec<String>, hunk: &[&str]) -> Result<(), String> {
    let mut old_block = Vec::new();
    let mut new_block = Vec::new();
    for line in hunk {
        let (tag, body) = line.split_at(1);
        match tag {
            " " => {
                old_block.push(body.to_string());
                new_block.push(body.to_string());
            }
            "-" => old_block.push(body.to_string()),
            "+" => new_block.push(body.to_string()),
            _ => return Err(format!("local_ide patch invalid hunk line: {line}")),
        }
    }
    if old_block.is_empty() {
        current.extend(new_block);
        return Ok(());
    }
    let pos = current
        .windows(old_block.len())
        .position(|window| window == old_block.as_slice())
        .ok_or_else(|| "local_ide patch context not found in target file".to_string())?;
    current.splice(pos..pos + old_block.len(), new_block);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_returns_canonical_diff_and_before_after_content() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("example.txt");
        fs::write(&path, "alpha\nbeta\n").expect("write seed");

        let result = op_patch(&LocalIdeArgs {
            op: "patch".to_string(),
            path: Some(path.to_string_lossy().to_string()),
            pattern: None,
            patch_text: Some("@@ ... @@\n alpha\n-beta\n+gamma\n".to_string()),
            limit: None,
            max_chars: None,
            lines: None,
        })
        .expect("patch succeeds");

        assert_eq!(result["ok"], true);
        assert_eq!(result["path"], path.to_string_lossy().replace('\\', "/"));
        assert_eq!(result["absolute_path"], path.to_string_lossy().replace('\\', "/"));
        assert_eq!(result["before_content"], "alpha\nbeta\n");
        assert_eq!(result["after_content"], "alpha\ngamma\n");
        assert_eq!(result["lines_added"], 1);
        assert_eq!(result["lines_removed"], 1);
        assert_eq!(result["no_op"], false);
        assert_eq!(result["normalized_patch_text"], "@@ ... @@\n alpha\n-beta\n+gamma");
        let applied = result["applied_patch_text"].as_str().expect("applied patch text");
        assert!(applied.contains("diff --git"));
        assert!(applied.contains("-beta"));
        assert!(applied.contains("+gamma"));
        assert_eq!(fs::read_to_string(&path).expect("read updated"), "alpha\ngamma\n");
    }

    #[test]
    fn patch_rejects_begin_patch_envelope() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("example.txt");
        fs::write(&path, "old\n").expect("write seed");

        let err = op_patch(&LocalIdeArgs {
            op: "patch".to_string(),
            path: Some(path.to_string_lossy().to_string()),
            pattern: None,
            patch_text: Some(
                "*** Begin Patch\n*** Update File: example.txt\n@@ ... @@\n- old\n+ new\n*** End Patch"
                    .to_string(),
            ),
            limit: None,
            max_chars: None,
            lines: None,
        })
        .expect_err("Begin Patch envelope should fail");

        assert!(err.contains("must not include a Begin Patch envelope"));
        assert_eq!(fs::read_to_string(&path).expect("read unchanged"), "old\n");
    }
}
