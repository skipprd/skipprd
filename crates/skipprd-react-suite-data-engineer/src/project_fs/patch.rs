use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::sync::Arc;

use crate::patch_contract::normalize_hunks_only_patch_text;
use crate::providers::DatasetCatalogProvider;
use diffy::Patch;
use react_core::agent::AgentCtx;

#[derive(Debug)]
pub struct PatchOutcome {
    pub rel_path: String,
    pub key: String,
    pub existed: bool,
    pub base_sha256: String,
    pub new_sha256: String,
    /// Canonical git-style unified diff (single-file).
    pub git_patch: String,
    pub diff: String,
    pub lines_added: usize,
    pub lines_removed: usize,
    pub content: String,
    pub apply_result_code: PatchApplyResultCode,
    pub apply_repairs: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchApplyResultCode {
    AppliedUnifiedDirect,
    AppliedHunksFlexible,
    AppliedUnifiedAfterHeaderRepair,
    AppliedUnifiedByFullReplacementReconstruction,
    AppliedUnifiedByFlexibleFallback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchApplyKind {
    /// Apply a git-style unified diff (possibly with git preamble).
    UnifiedDiff,
}

#[cfg(test)]
pub fn create_patch_text(old: &str, new: &str) -> String {
    diffy::create_patch(old, new).to_string()
}

/// Create a Cursor/Aider-style hunks-only "full replacement" patch.
///
/// This avoids line-number hunks entirely and is intended for deterministic/internal authors that
/// already produced final file text but still want to go through the unified patch apply path.
pub fn hunks_only_full_replace_patch(old: &str, new: &str) -> String {
    let (old_lines, _old_nl) = split_lines_preserve_trailing_newline(old);
    let (new_lines, _new_nl) = split_lines_preserve_trailing_newline(new);
    let mut out = String::new();
    out.push_str("@@ ... @@\n");
    for l in old_lines {
        out.push('-');
        out.push_str(&l);
        out.push('\n');
    }
    for l in new_lines {
        out.push('+');
        out.push_str(&l);
        out.push('\n');
    }
    out
}

/// Create a git-style unified diff for a single file.
///
/// This is intended for deterministic/internal patch generation (not LLM authoring).
/// For new files, it uses `--- /dev/null` which is required by the patch protocol.
pub fn create_git_patch_text(
    old: &str,
    new: &str,
    rel_path: &str,
    existed: bool,
) -> Result<String, String> {
    let rel = normalize_rel_path(rel_path)?;
    let base = diffy::create_patch(old, new).to_string();
    if base.lines().take(2).count() < 2 {
        return Err("failed to generate patch: missing header lines".to_string());
    }

    let header_old = if existed {
        format!("--- a/{}", rel)
    } else {
        "--- /dev/null".to_string()
    };
    let header_new = format!("+++ b/{}", rel);
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
    Ok(out.join("\n"))
}

fn split_lines_preserve_trailing_newline(s: &str) -> (Vec<String>, bool) {
    let had_trailing_newline = s.ends_with('\n');
    let mut lines: Vec<String> = s.split('\n').map(|x| x.to_string()).collect();
    if had_trailing_newline {
        if let Some(last) = lines.last() {
            if last.is_empty() {
                lines.pop();
            }
        }
    }
    (lines, had_trailing_newline)
}

fn join_lines_preserve_trailing_newline(lines: &[String], had_trailing_newline: bool) -> String {
    let mut out = lines.join("\n");
    if had_trailing_newline {
        out.push('\n');
    }
    out
}

/// Apply a 1-based inclusive line replacement to a text blob.
///
/// Supports insertion by specifying `start_line == end_line + 1`.
#[cfg(test)]
pub fn apply_replace_range(
    old_text: &str,
    start_line: usize,
    end_line: usize,
    new_text: &str,
) -> Result<String, String> {
    let (mut lines, had_trailing_newline) = split_lines_preserve_trailing_newline(old_text);
    let n = lines.len();
    if start_line == 0 {
        return Err("start_line must be >= 1".to_string());
    }
    let end_line = end_line.min(n);
    if start_line > n + 1 {
        return Err(format!(
            "start_line out of bounds: {} > {}",
            start_line,
            n + 1
        ));
    }
    if start_line > end_line + 1 {
        return Err(format!(
            "invalid range: start_line {} > end_line {} + 1",
            start_line, end_line
        ));
    }

    let start_idx = start_line - 1;
    let end_idx_excl = end_line;

    let mut new_lines: Vec<String> = new_text.split('\n').map(|x| x.to_string()).collect();
    if !new_text.ends_with('\n') {}

    lines.splice(start_idx..end_idx_excl, new_lines.drain(..));
    Ok(join_lines_preserve_trailing_newline(
        &lines,
        had_trailing_newline,
    ))
}

#[cfg(test)]
pub fn apply_replace_list(
    old_text: &str,
    edits: &[super::ReplaceListEdit],
) -> Result<String, String> {
    if edits.is_empty() {
        return Ok(old_text.to_string());
    }
    let mut sorted: Vec<super::ReplaceListEdit> = edits.to_vec();
    sorted.sort_by(|a, b| {
        b.start_line
            .cmp(&a.start_line)
            .then_with(|| b.end_line.cmp(&a.end_line))
    });

    let mut cur = old_text.to_string();
    for e in sorted.iter() {
        cur = apply_replace_range(&cur, e.start_line, e.end_line, &e.new_text)?;
    }
    Ok(cur)
}

pub fn normalize_rel_path(rel: &str) -> Result<String, String> {
    let rel = rel.trim().trim_start_matches("./").to_string();
    if !super::is_allowed_rel_path(&rel) {
        return Err("path not allowed; only relative paths within the dbt project are permitted (no absolute paths, no '..' traversal)".to_string());
    }
    Ok(rel.replace('\\', "/"))
}

pub async fn apply_patch(
    ctx: &AgentCtx,
    datasets: Option<&Arc<dyn DatasetCatalogProvider>>,
    path: &str,
    payload: &str,
    base_sha256: Option<&str>,
    expected_existed: Option<bool>,
    kind: PatchApplyKind,
) -> Result<PatchOutcome, String> {
    let rel = normalize_rel_path(path)?;
    let key = super::join_storage_key(ctx, &rel);
    let existing = ctx
        .storage()
        .get_bytes(&key)
        .await
        .ok()
        .map(|b| String::from_utf8_lossy(&b).to_string());
    let existed = existing.is_some();
    let old = existing.unwrap_or_default();
    let base_hash = super::sha256_hex(&old);
    if let Some(want_existed) = expected_existed {
        if want_existed != existed {
            return Err(format!(
                "base_exists mismatch; expected existed={}, got existed={}",
                want_existed, existed
            ));
        }
    }
    if let Some(expected) = base_sha256 {
        if expected != base_hash {
            return Err(format!(
                "base_sha256 mismatch; expected {}, got {}",
                expected, base_hash
            ));
        }
    }

    let mut apply_result_code = PatchApplyResultCode::AppliedUnifiedDirect;
    let mut apply_repairs: Vec<String> = Vec::new();
    let mut new_content = match kind {
        PatchApplyKind::UnifiedDiff => {
            let patch_text = payload;
            let has_file_headers = patch_text
                .lines()
                .any(|l| l.trim_start().starts_with("--- ") || l.trim_start().starts_with("+++ "));
            let has_hunks = patch_text.lines().any(|l| l.trim_start().starts_with("@@"));

            if has_hunks && !has_file_headers {
                let normalized = normalize_hunks_only_patch_text(patch_text, &rel)
                    .map_err(|e| format!("invalid patch: {}", e))?;
                if normalized.line_number_headers_rewritten > 0 {
                    apply_repairs.push(format!(
                        "normalized_line_number_hunk_headers={}",
                        normalized.line_number_headers_rewritten
                    ));
                }
                if normalized.git_headers_stripped {
                    apply_repairs.push("stripped_git_file_headers".to_string());
                }
                if normalized.git_metadata_lines_dropped > 0 {
                    apply_repairs.push(format!(
                        "dropped_git_metadata_lines={}",
                        normalized.git_metadata_lines_dropped
                    ));
                }
                if let Some(repl) =
                    try_apply_unified_hunks_flexible(normalized.patch_text.as_str(), &old)
                {
                    apply_result_code = PatchApplyResultCode::AppliedHunksFlexible;
                    repl
                } else {
                    return Err("patch_hunk_context_miss: hunks-only patch could not be applied to current file content".to_string());
                }
            } else {
                let is_new_file_patch = patch_text.lines().any(|l| l.trim() == "--- /dev/null");
                if existed && is_new_file_patch {
                    return Err("invalid patch: patch indicates new file creation ('--- /dev/null') but the file already exists; produce a normal edit patch against the existing path".to_string());
                }
                if existed
                    && patch_text
                        .lines()
                        .any(|l| l.trim_start().starts_with("new file mode "))
                {
                    return Err("invalid patch: patch indicates new file creation ('new file mode') but the file already exists; produce a normal edit patch against the existing path".to_string());
                }

                let unified = strip_git_preamble_to_unified(patch_text)?;
                let mut patch_src: Cow<'_, str> = Cow::Borrowed(&unified);
                let mut parse_error_fallback_content: Option<String> = None;
                let patch = match Patch::from_str(patch_src.as_ref()) {
                    Ok(p) => p,
                    Err(e) => {
                        let emsg = e.to_string();
                        let fixed = repair_unified_hunk_headers(&unified);
                        if fixed != unified {
                            patch_src = Cow::Owned(fixed);
                            apply_repairs.push("repaired_unified_hunk_headers".to_string());
                            match Patch::from_str(patch_src.as_ref()) {
                                Ok(p) => p,
                                Err(e2) => {
                                    if e2.to_string().contains("unable to parse hunk header") {
                                        if let Some(repl) = try_apply_unified_hunks_flexible(
                                            patch_src.as_ref(),
                                            &old,
                                        ) {
                                            apply_result_code =
                                                PatchApplyResultCode::AppliedUnifiedByFlexibleFallback;
                                            parse_error_fallback_content = Some(repl);
                                            Patch::from_str("--- a/x\n+++ b/x\n@@ -1,0 +1,0 @@\n")
                                                .map_err(|_| format!("invalid patch: {}", e2))?
                                        } else {
                                            return Err(format!("invalid patch: {}", e2));
                                        }
                                    } else {
                                        return Err(format!("invalid patch: {}", e2));
                                    }
                                }
                            }
                        } else {
                            if emsg.contains("unable to parse hunk header") {
                                if let Some(repl) = try_apply_unified_hunks_flexible(&unified, &old)
                                {
                                    apply_result_code =
                                        PatchApplyResultCode::AppliedUnifiedByFlexibleFallback;
                                    parse_error_fallback_content = Some(repl);
                                    Patch::from_str("--- a/x\n+++ b/x\n@@ -1,0 +1,0 @@\n")
                                        .map_err(|_| format!("invalid patch: {}", emsg))?
                                } else {
                                    return Err(format!("invalid patch: {}", emsg));
                                }
                            } else {
                                return Err(format!("invalid patch: {}", emsg));
                            }
                        }
                    }
                };
                if let Some(repl) = parse_error_fallback_content {
                    repl
                } else {
                    match diffy::apply(&old, &patch) {
                        Ok(c) => {
                            if apply_repairs
                                .iter()
                                .any(|x| x == "repaired_unified_hunk_headers")
                            {
                                apply_result_code =
                                    PatchApplyResultCode::AppliedUnifiedAfterHeaderRepair;
                            } else {
                                apply_result_code = PatchApplyResultCode::AppliedUnifiedDirect;
                            }
                            c
                        }
                        Err(e) => return Err(format!("patch apply failed: {}", e)),
                    }
                }
            }
        }
    };
    new_content = super::yaml::postprocess_content(ctx, datasets, &rel, &new_content).await?;

    let git_patch = create_git_patch_text(&old, &new_content, &rel, existed)?;
    let diff = super::diff::compute_unified_diff(&old, &new_content);
    let (lines_added, lines_removed) = super::diff::diff_stats(&old, &new_content);
    let new_hash = super::sha256_hex(&new_content);
    Ok(PatchOutcome {
        rel_path: rel,
        key,
        existed,
        base_sha256: base_hash,
        new_sha256: new_hash,
        git_patch,
        diff,
        lines_added,
        lines_removed,
        content: new_content,
        apply_result_code,
        apply_repairs,
    })
}

fn parse_hunk_range(part: &str, sign: char) -> Option<(usize, usize)> {
    let p = part.trim();
    let p = p.strip_prefix(sign)?;
    let mut it = p.splitn(2, ',');
    let start = it.next()?.trim().parse::<usize>().ok()?;
    let count = match it.next() {
        Some(c) => c.trim().parse::<usize>().ok()?,
        None => 1usize,
    };
    Some((start, count))
}

fn parse_hunk_marker_line(line: &str) -> Option<(char, &str)> {
    let mut chars = line.chars();
    let marker = chars.next()?;
    if matches!(marker, '+' | '-' | ' ') {
        Some((marker, chars.as_str()))
    } else {
        None
    }
}

fn leading_ws_width(s: &str) -> usize {
    s.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

fn strip_common_leading_ws(lines: &[String]) -> Vec<String> {
    let min_ws = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| leading_ws_width(l))
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            if l.len() <= min_ws {
                String::new()
            } else {
                l.chars().skip(min_ws).collect::<String>()
            }
        })
        .collect()
}

fn find_block_index_exact(hay: &[String], needle: &[String]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > hay.len() {
        return None;
    }
    for i in 0..=(hay.len() - needle.len()) {
        if hay[i..i + needle.len()] == *needle {
            return Some(i);
        }
    }
    None
}

fn find_block_index_trimmed(hay: &[String], needle: &[String]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > hay.len() {
        return None;
    }
    let needle_t: Vec<String> = needle.iter().map(|s| s.trim_end().to_string()).collect();
    for i in 0..=(hay.len() - needle.len()) {
        let cand: Vec<String> = hay[i..i + needle.len()]
            .iter()
            .map(|s| s.trim_end().to_string())
            .collect();
        if cand == needle_t {
            return Some(i);
        }
    }
    None
}

fn find_block_index_relative_ws(hay: &[String], needle: &[String]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > hay.len() {
        return None;
    }
    let needle_d = strip_common_leading_ws(needle);
    for i in 0..=(hay.len() - needle.len()) {
        let cand: Vec<String> = hay[i..i + needle.len()].to_vec();
        if strip_common_leading_ws(&cand) == needle_d {
            return Some(i);
        }
    }
    None
}

fn find_block_index_flexible(hay: &[String], needle: &[String]) -> Option<usize> {
    find_block_index_exact(hay, needle)
        .or_else(|| find_block_index_trimmed(hay, needle))
        .or_else(|| find_block_index_relative_ws(hay, needle))
}

fn apply_hunk_search_replace_flexible(
    current: &str,
    old_block: &[String],
    new_block: &[String],
) -> Option<String> {
    let (mut lines, had_trailing_newline) = split_lines_preserve_trailing_newline(current);
    let old_side = old_block.to_vec();
    let new_side = new_block.to_vec();
    if old_side.is_empty() && new_side.is_empty() {
        return Some(current.to_string());
    }

    if let Some(idx) = find_block_index_flexible(&lines, &old_side) {
        lines.splice(idx..idx + old_side.len(), new_side);
        return Some(join_lines_preserve_trailing_newline(
            &lines,
            had_trailing_newline,
        ));
    }

    let mut prefix = 0usize;
    let mut suffix = 0usize;
    while prefix < old_side.len() && prefix < new_side.len() && old_side[prefix] == new_side[prefix]
    {
        prefix += 1;
    }
    while suffix + prefix < old_side.len()
        && suffix + prefix < new_side.len()
        && old_side[old_side.len().saturating_sub(1 + suffix)]
            == new_side[new_side.len().saturating_sub(1 + suffix)]
    {
        suffix += 1;
    }
    if prefix > 0 || suffix > 0 {
        let old_core_end = old_side.len().saturating_sub(suffix);
        let new_core_end = new_side.len().saturating_sub(suffix);
        if prefix <= old_core_end && prefix <= new_core_end {
            let old_core = old_side[prefix..old_core_end].to_vec();
            let new_core = new_side[prefix..new_core_end].to_vec();
            if let Some(idx) = find_block_index_flexible(&lines, &old_core) {
                lines.splice(idx..idx + old_core.len(), new_core);
                return Some(join_lines_preserve_trailing_newline(
                    &lines,
                    had_trailing_newline,
                ));
            }
        }
    }
    None
}

/// Best-effort Cursor/Aider-style hunk applier.
///
/// Interprets each hunk body as search/replace:
/// - old/search side: ' ' + '-'
/// - new/replace side: ' ' + '+'
/// - accepts malformed lines without marker as shared context on both sides
fn try_apply_unified_hunks_flexible(unified: &str, old: &str) -> Option<String> {
    let lines: Vec<&str> = unified.lines().collect();
    if !lines.iter().any(|l| l.starts_with("@@")) {
        return None;
    }
    let mut out = old.to_string();
    let mut i = 0usize;
    let mut applied_any = false;
    while i < lines.len() {
        if !lines[i].starts_with("@@") {
            i += 1;
            continue;
        }
        i += 1;
        let mut old_block: Vec<String> = Vec::new();
        let mut new_block: Vec<String> = Vec::new();
        let mut changed = false;
        while i < lines.len() && !lines[i].starts_with("@@") {
            let l = lines[i];
            if l.starts_with('\\') {
                i += 1;
                continue;
            }
            match parse_hunk_marker_line(l) {
                Some(('-', body)) => {
                    old_block.push(body.to_string());
                    changed = true;
                }
                Some(('+', body)) => {
                    new_block.push(body.to_string());
                    changed = true;
                }
                Some((' ', body)) => {
                    old_block.push(body.to_string());
                    new_block.push(body.to_string());
                }
                _ => {
                    old_block.push(l.to_string());
                    new_block.push(l.to_string());
                }
            }
            i += 1;
        }
        if !changed {
            continue;
        }
        let next = apply_hunk_search_replace_flexible(&out, &old_block, &new_block)?;
        out = next;
        applied_any = true;
    }
    if applied_any {
        Some(out)
    } else {
        None
    }
}

/// Repair unified diff hunks by recomputing their line counts from the hunk bodies.
///
/// This is a best-effort normalization for strict patch parsers (diffy). It preserves hunk start
/// positions, and only rewrites the `-a,b +c,d` counts to match the actual hunk body.
fn repair_unified_hunk_headers(unified: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let lines: Vec<&str> = unified.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        if !line.starts_with("@@") {
            out.push(line.to_string());
            i += 1;
            continue;
        }

        let after = line.trim_start_matches("@@").trim_start();
        let Some(end_idx) = after.find("@@") else {
            out.push(line.to_string());
            i += 1;
            continue;
        };
        let ranges = after[..end_idx].trim();
        let suffix = &after[end_idx + 2..];
        let parts: Vec<&str> = ranges.split_whitespace().collect();
        if parts.len() < 2 {
            out.push(line.to_string());
            i += 1;
            continue;
        }
        let Some((old_start, _old_count_decl)) = parse_hunk_range(parts[0], '-') else {
            out.push(line.to_string());
            i += 1;
            continue;
        };
        let Some((new_start, _new_count_decl)) = parse_hunk_range(parts[1], '+') else {
            out.push(line.to_string());
            i += 1;
            continue;
        };

        let mut old_count = 0usize;
        let mut new_count = 0usize;
        let mut j = i + 1;
        while j < lines.len() {
            let l = lines[j];
            if l.starts_with("@@") {
                break;
            }
            if l.starts_with('\\') {
                j += 1;
                continue;
            }
            match l.chars().next().unwrap_or(' ') {
                '-' => old_count += 1,
                '+' => new_count += 1,
                ' ' => {
                    old_count += 1;
                    new_count += 1;
                }
                _ => {}
            }
            j += 1;
        }

        let fixed = format!("@@ -{old_start},{old_count} +{new_start},{new_count} @@{suffix}");
        out.push(fixed);
        i += 1;
        continue;
    }
    out.join("\n")
}

fn strip_git_preamble_to_unified(patch_chunk: &str) -> Result<String, String> {
    let mut out: Vec<String> = Vec::new();
    let mut started = false;
    for line in patch_chunk.lines() {
        let t = line.trim_end_matches('\r');
        if !started {
            if t.starts_with("--- ") {
                started = true;
                out.push(t.to_string());
            }
            continue;
        }
        if t.starts_with("diff --git ")
            || t.starts_with("index ")
            || t.starts_with("new file mode ")
            || t.starts_with("deleted file mode ")
            || t.starts_with("similarity index ")
            || t.starts_with("rename from ")
            || t.starts_with("rename to ")
        {
            continue;
        }
        out.push(t.to_string());
    }
    if out.is_empty() {
        return Err(
            "invalid patch bundle: could not find unified diff header line starting with '--- '"
                .to_string(),
        );
    }
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_fs::join_storage_key;
    use crate::project_fs::test_helpers::*;
    use crate::providers::DatasetId;
    use serde_yaml::Value as YamlValue;

    #[tokio::test]
    async fn apply_patch_creates_file_and_injects_config() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(react_module_storage_memory::InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage, None);
        let sql = "select * from {{ source('test_raw','raw_orders') }}";
        let patch_text =
            create_git_patch_text("", sql, "models/staging/stg_test_raw_raw_orders.sql", false)
                .expect("patch");
        let outcome = apply_patch(
            &ctx,
            None,
            "models/staging/stg_test_raw_raw_orders.sql",
            &patch_text,
            None,
            None,
            PatchApplyKind::UnifiedDiff,
        )
        .await
        .expect("apply patch");
        assert!(outcome
            .content
            .contains("config(alias=\"stg_test_raw_raw_orders\""));
        assert!(outcome
            .content
            .to_ascii_lowercase()
            .contains("source('test_raw','raw_orders')"));
    }

    #[tokio::test]
    async fn apply_patch_rejects_misnamed_staging_model_path() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(react_module_storage_memory::InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage, None);
        let sql = "select * from {{ source('test_raw','raw_orders') }}";
        let patch_text =
            create_git_patch_text("", sql, "models/staging/stg_wrong.sql", false).expect("patch");
        let err = apply_patch(
            &ctx,
            None,
            "models/staging/stg_wrong.sql",
            &patch_text,
            None,
            None,
            PatchApplyKind::UnifiedDiff,
        )
        .await
        .unwrap_err();
        assert!(err.contains("must be written to"));
        assert!(err.contains("models/staging/stg_test_raw_raw_orders.sql"));
    }

    #[tokio::test]
    async fn apply_patch_rejects_staging_with_multiple_sources() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(react_module_storage_memory::InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage, None);
        let sql = r#"
select * from {{ source('test_raw','raw_orders') }}
union all
select * from {{ source('test_raw','raw_customers') }}
"#;
        let patch_text =
            create_git_patch_text("", sql, "models/staging/stg_test_raw_raw_orders.sql", false)
                .expect("patch");
        let err = apply_patch(
            &ctx,
            None,
            "models/staging/stg_test_raw_raw_orders.sql",
            &patch_text,
            None,
            None,
            PatchApplyKind::UnifiedDiff,
        )
        .await
        .unwrap_err();
        assert!(err.contains("exactly one dbt source() call"));
        assert!(err.contains("found 2"));
    }

    #[tokio::test]
    async fn apply_patch_rejects_gold_model_using_source() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(react_module_storage_memory::InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage, None);
        let sql = "select * from {{ source('test_raw','raw_orders') }}";
        let patch_text =
            create_git_patch_text("", sql, "models/marts/fct_orders.sql", false).expect("patch");
        let err = apply_patch(
            &ctx,
            None,
            "models/marts/fct_orders.sql",
            &patch_text,
            None,
            None,
            PatchApplyKind::UnifiedDiff,
        )
        .await
        .unwrap_err();
        assert!(err.contains("gold models must NOT reference dbt source()"));
    }

    #[tokio::test]
    async fn apply_patch_schema_yml_rebuilds_sources_and_preserves_models() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(react_module_storage_memory::InMemoryStorageAdapter::default());
        let q = MockQuery::default();
        *q.schemas.lock().unwrap() = std::collections::HashMap::from([
            (
                "AwsDataCatalog.test_raw.raw_customers".to_string(),
                vec![("id".to_string(), "varchar".to_string())],
            ),
            (
                "AwsDataCatalog.test_raw.raw_orders".to_string(),
                vec![("id".to_string(), "varchar".to_string())],
            ),
        ]);
        let ctx = make_ctx(storage, Some(Arc::new(q)));
        let datasets: Arc<dyn crate::providers::DatasetCatalogProvider> = Arc::new(MockDatasets {
            items: vec![
                DatasetId {
                    catalog: "AwsDataCatalog".to_string(),
                    database: "test_raw".to_string(),
                    table: "raw_customers".to_string(),
                },
                DatasetId {
                    catalog: "AwsDataCatalog".to_string(),
                    database: "test_raw".to_string(),
                    table: "raw_orders".to_string(),
                },
            ],
        });
        let existing = "version: 2\nmodels:\n  - name: stg_raw_customers\n";
        let patch_text =
            create_git_patch_text("", existing, "models/schema.yml", false).expect("patch");
        let outcome = apply_patch(
            &ctx,
            Some(&datasets),
            "models/schema.yml",
            &patch_text,
            None,
            None,
            PatchApplyKind::UnifiedDiff,
        )
        .await
        .expect("apply patch");
        let v: YamlValue = serde_yaml::from_str(&outcome.content).expect("valid yaml");
        let map = v.as_mapping().expect("mapping root");
        assert!(map.contains_key(&YamlValue::String("models".to_string())));
        assert!(map.contains_key(&YamlValue::String("sources".to_string())));
    }

    #[test]
    fn replace_range_replaces_middle_and_preserves_trailing_newline() {
        let old = "a\nb\nc\nd\n";
        let out = apply_replace_range(old, 2, 3, "X\nY").expect("replace");
        assert_eq!(out, "a\nX\nY\nd\n");
    }

    #[test]
    fn replace_range_inserts_at_start_with_end_line_zero() {
        let old = "b\nc\n";
        let out = apply_replace_range(old, 1, 0, "a").expect("insert");
        assert_eq!(out, "a\nb\nc\n");
    }

    #[test]
    fn replace_range_clamps_end_line_to_eof() {
        let old = "a\nb\nc\n";
        let out = apply_replace_range(old, 2, 1000, "X").expect("replace");
        assert_eq!(out, "a\nX\n");
    }

    #[test]
    fn replace_range_allows_insert_at_eof_with_oversized_end_line() {
        let old = "a\n";
        let out = apply_replace_range(old, 2, 1000, "b").expect("insert");
        assert_eq!(out, "a\nb\n");
    }

    #[tokio::test]
    async fn apply_patch_repairs_hunk_counts_for_new_file() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(react_module_storage_memory::InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage, None);

        let rel = "models/staging/stg_test_raw_raw_orders.sql";
        let patch_text = [
            "diff --git a/models/staging/stg_test_raw_raw_orders.sql b/models/staging/stg_test_raw_raw_orders.sql",
            "new file mode 100644",
            "--- /dev/null",
            "+++ b/models/staging/stg_test_raw_raw_orders.sql",
            "@@ -0,0 +1,99 @@",
            "+select *",
            "+from {{ source('test_raw','raw_orders') }}",
        ]
        .join("\n");

        let outcome = apply_patch(
            &ctx,
            None,
            rel,
            &patch_text,
            None,
            None,
            PatchApplyKind::UnifiedDiff,
        )
        .await
        .expect("apply patch");
        assert!(outcome.content.contains("source('test_raw','raw_orders')"));
    }

    #[tokio::test]
    async fn apply_patch_packages_yml_normalizes_entries() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(react_module_storage_memory::InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage, None);
        let raw = r#"
packages:
  - package: calogica/dbt_expectations
    version: [">=0.8.0", "<1.0.0"]
  - package: dbt-labs/dbt_utils
    version: [">=1.0.0", "<2.0.0"]
  - package: dbt-labs/dbt_utils
    version: [">=1.0.0", "<2.0.0"]
"#;
        let patch_text = create_git_patch_text("", raw, "packages.yml", false).expect("patch");
        let outcome = apply_patch(
            &ctx,
            None,
            "packages.yml",
            &patch_text,
            None,
            None,
            PatchApplyKind::UnifiedDiff,
        )
        .await
        .expect("apply patch");
        let v: YamlValue = serde_yaml::from_str(&outcome.content).expect("valid yaml");
        let map = v.as_mapping().expect("mapping root");
        let packages = map
            .get(&YamlValue::String("packages".to_string()))
            .and_then(|v| v.as_sequence())
            .expect("packages list");
        assert_eq!(packages.len(), 2);
        let first = packages[0].as_mapping().expect("mapping");
        let first_pkg = first
            .get(&YamlValue::String("package".to_string()))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(first_pkg, "calogica/dbt_expectations");
    }

    #[tokio::test]
    async fn apply_patch_rejects_base_sha_mismatch() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(react_module_storage_memory::InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage, None);
        let patch_text =
            create_git_patch_text("", "select 1", "models/staging/stg_orders.sql", false)
                .expect("patch");
        let err = apply_patch(
            &ctx,
            None,
            "models/staging/stg_orders.sql",
            &patch_text,
            Some("bad"),
            None,
            PatchApplyKind::UnifiedDiff,
        )
        .await
        .unwrap_err();
        assert!(err.contains("base_sha256 mismatch"));
    }

    #[tokio::test]
    async fn apply_patch_accepts_cursor_hunk_header_ellipsis() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(react_module_storage_memory::InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage.clone(), None);
        let rel = "macros/helpers.sql";
        let key = join_storage_key(&ctx, rel);
        ctx.storage()
            .put_bytes(&key, b"select 1\n", "text/sql")
            .await
            .expect("seed");
        let patch_text = [
            "diff --git a/macros/helpers.sql b/macros/helpers.sql",
            "--- a/macros/helpers.sql",
            "+++ b/macros/helpers.sql",
            "@@ ... @@",
            "-select 1",
            "+select 2",
        ]
        .join("\n");
        let out = apply_patch(
            &ctx,
            None,
            rel,
            &patch_text,
            None,
            None,
            PatchApplyKind::UnifiedDiff,
        )
        .await
        .expect("apply");
        assert!(out.content.contains("select 2"));
    }

    #[tokio::test]
    async fn apply_patch_flexible_hunk_matching_handles_indent_drift() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(react_module_storage_memory::InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage.clone(), None);
        let rel = "macros/helpers.sql";
        let key = join_storage_key(&ctx, rel);
        let seed = "fn x() {\n    if ok {\n        return 1;\n    }\n}\n";
        ctx.storage()
            .put_bytes(&key, seed.as_bytes(), "text/sql")
            .await
            .expect("seed");
        let patch_text = [
            "diff --git a/macros/helpers.sql b/macros/helpers.sql",
            "--- a/macros/helpers.sql",
            "+++ b/macros/helpers.sql",
            "@@ ... @@",
            " fn x() {",
            "-    if ok {",
            "-        return 1;",
            "-    }",
            "+if ok {",
            "+    return 2;",
            "+}",
            " }",
        ]
        .join("\n");
        let out = apply_patch(
            &ctx,
            None,
            rel,
            &patch_text,
            None,
            None,
            PatchApplyKind::UnifiedDiff,
        )
        .await
        .expect("apply");
        assert!(out.content.contains("return 2;"));
    }

    #[tokio::test]
    async fn apply_patch_accepts_hunks_only_for_new_file() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(react_module_storage_memory::InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage, None);
        let rel = "models/core/x.sql";
        let patch_text = "@@ ... @@\n+select 1\n";
        let out = apply_patch(
            &ctx,
            None,
            rel,
            patch_text,
            None,
            None,
            PatchApplyKind::UnifiedDiff,
        )
        .await
        .expect("apply");
        assert!(out.content.to_ascii_lowercase().contains("select 1"));
        assert!(out.content.contains("config(alias=\"x\""));
    }

    #[tokio::test]
    async fn apply_patch_accepts_hunks_only_for_existing_file() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(react_module_storage_memory::InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage.clone(), None);
        let rel = "models/core/x.sql";
        let key = join_storage_key(&ctx, rel);
        ctx.storage()
            .put_bytes(&key, b"select 1\n", "text/sql")
            .await
            .expect("seed");
        let patch_text = "@@ ... @@\n-select 1\n+select 2\n";
        let out = apply_patch(
            &ctx,
            None,
            rel,
            patch_text,
            None,
            None,
            PatchApplyKind::UnifiedDiff,
        )
        .await
        .expect("apply");
        assert!(out.content.to_ascii_lowercase().contains("select 2"));
    }

    #[tokio::test]
    async fn apply_patch_hunks_only_normalizes_line_number_headers() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(react_module_storage_memory::InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage.clone(), None);
        let rel = "models/core/x.sql";
        let key = join_storage_key(&ctx, rel);
        ctx.storage()
            .put_bytes(&key, b"select 1\n", "text/sql")
            .await
            .expect("seed");
        let patch_text = "@@ -1,1 +1,1 @@\n-select 1\n+select 2\n";
        let out = apply_patch(
            &ctx,
            None,
            rel,
            patch_text,
            None,
            None,
            PatchApplyKind::UnifiedDiff,
        )
        .await
        .expect("apply");
        assert!(out.content.to_ascii_lowercase().contains("select 2"));
        assert_eq!(
            out.apply_result_code,
            PatchApplyResultCode::AppliedHunksFlexible
        );
        assert!(out
            .apply_repairs
            .iter()
            .any(|r| r.starts_with("normalized_line_number_hunk_headers=")));
    }

    #[test]
    fn flexible_hunk_apply_tolerates_empty_unmarked_body_lines() {
        let old = "a\n\nb\n";
        let unified = "@@ -1,3 +1,3 @@\n a\n\n-b\n+c\n";
        let out = try_apply_unified_hunks_flexible(unified, old).expect("apply should succeed");
        assert_eq!(out, "a\n\nc\n");
    }
}
