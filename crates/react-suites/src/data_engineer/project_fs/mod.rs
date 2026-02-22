use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_yaml::{Mapping as YamlMapping, Value as YamlValue};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::Path;

use diffy::Patch;
use react_core::agent::AgentCtx;
use react_core::providers::{DatasetCatalogProvider, DatasetId};

use crate::data_engineer::naming;
use crate::data_engineer::patch_contract::normalize_hunks_only_patch_text;
use crate::data_engineer::project_files;

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

    // diffy uses: "--- original" and "+++ modified"
    // Replace with git-style file headers.
    // Build final output explicitly rather than trying to mutate &str slices.
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
    // Skip the first two diffy header lines.
    for l in base.lines().skip(2) {
        out.push(l.to_string());
    }
    Ok(out.join("\n"))
}

fn split_lines_preserve_trailing_newline(s: &str) -> (Vec<String>, bool) {
    let had_trailing_newline = s.ends_with('\n');
    let mut lines: Vec<String> = s.split('\n').map(|x| x.to_string()).collect();
    // `split('\n')` produces a trailing empty segment when the string ends with '\n'.
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
    // Treat `end_line > file_len` as "replace to EOF".
    // This is a common pattern in LLM-authored patches (e.g., end_line=1000) and is safe to
    // clamp since we already include `existing_line_count` in prompts/tool outputs.
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

    // Convert to 0-based indices in the current line vector.
    let start_idx = start_line - 1;
    let end_idx_excl = end_line; // inclusive end_line -> exclusive index in 0-based vec

    let mut new_lines: Vec<String> = new_text.split('\n').map(|x| x.to_string()).collect();
    // Preserve explicit trailing newline in the replacement block as an extra empty line.
    // (split already keeps the trailing empty segment).
    if !new_text.ends_with('\n') {
        // If there was no trailing newline, `split` will not have produced a trailing empty segment.
        // This is fine; no change required.
    }

    lines.splice(start_idx..end_idx_excl, new_lines.drain(..));
    Ok(join_lines_preserve_trailing_newline(
        &lines,
        had_trailing_newline,
    ))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceListEdit {
    pub start_line: usize,
    pub end_line: usize,
    pub new_text: String,
}

/// Apply multiple 1-based inclusive line replacements to a text blob.
///
/// Semantics:
/// - Each edit is interpreted against the *original* line numbering by applying edits
///   in descending start_line order (edits higher in the file are applied first).
/// - Insertions are supported via `start_line == end_line + 1` (same as apply_replace_range).
pub fn apply_replace_list(old_text: &str, edits: &[ReplaceListEdit]) -> Result<String, String> {
    if edits.is_empty() {
        return Ok(old_text.to_string());
    }
    let mut sorted: Vec<ReplaceListEdit> = edits.to_vec();
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
    if !is_allowed_rel_path(&rel) {
        return Err("path not allowed; only relative paths within the dbt project are permitted (no absolute paths, no '..' traversal)".to_string());
    }
    Ok(rel.replace('\\', "/"))
}

pub fn join_storage_key(ctx: &AgentCtx, rel: &str) -> String {
    let base = ctx
        .keyspace
        .dbt_prefix(&ctx.scope)
        .trim_end_matches('/')
        .to_string();
    format!("{}/{}", base, rel)
}

pub async fn list_files(ctx: &AgentCtx, prefix: &str, limit: usize) -> Result<Value, String> {
    let rel_prefix = if prefix.trim().is_empty() {
        "models/".to_string()
    } else {
        normalize_rel_path(prefix)?
    };
    let key_prefix = join_storage_key(ctx, &rel_prefix.trim_start_matches('/'));
    let mut keys = ctx
        .storage
        .list_prefix(&key_prefix)
        .await
        .unwrap_or_default();
    keys.sort();
    let mut out: Vec<Value> = Vec::new();
    for k in keys.into_iter().take(limit) {
        let rel = k
            .strip_prefix(
                &(ctx
                    .keyspace
                    .dbt_prefix(&ctx.scope)
                    .trim_end_matches('/')
                    .to_string()
                    + "/"),
            )
            .unwrap_or(&k)
            .to_string();
        out.push(serde_json::json!({"path": rel, "key": k}));
    }
    Ok(serde_json::json!({"ok": true, "items": out}))
}

pub async fn get_file(ctx: &AgentCtx, path: &str, max_chars: usize) -> Result<Value, String> {
    let rel = normalize_rel_path(path)?;
    let key = join_storage_key(ctx, &rel);
    match ctx.storage.get_bytes(&key).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes).to_string();
            // Grounding metadata for safe patching.
            let base_sha256 = {
                use sha2::Digest;
                let mut hasher = sha2::Sha256::new();
                hasher.update(text.as_bytes());
                hex::encode(hasher.finalize())
            };
            let existing_line_count = if text.is_empty() { 0 } else { text.lines().count() };
            let existing_had_trailing_newline = text.ends_with('\n');
            let content = if max_chars > 0 && text.len() > max_chars {
                let mut s = text.chars().take(max_chars).collect::<String>();
                s.push_str("\n... (truncated; use dbt_files op=get_json or op=manifest_find for structured access)\n");
                s
            } else {
                text
            };
            Ok(serde_json::json!({
                "ok": true,
                "path": rel,
                "key": key,
                "base_sha256": base_sha256,
                "existing_line_count": existing_line_count,
                "existing_had_trailing_newline": existing_had_trailing_newline,
                "content": content
            }))
        }
        Err(e) => {
            let err_text = e.to_string();
            let bootstrap_missing = (rel == project_files::PACKAGES_YML
                || rel == project_files::MODELS_SCHEMA_YML)
                && is_missing_storage_error(&err_text);
            if bootstrap_missing {
                return Ok(serde_json::json!({
                    "ok": true,
                    "path": rel,
                    "key": key,
                    "exists": false,
                    "missing": true,
                    "base_sha256": "",
                    "existing_line_count": 0,
                    "existing_had_trailing_newline": false,
                    "content": "",
                }));
            }
            Ok(serde_json::json!({
                "ok": false,
                "path": rel,
                "key": key,
                "error": format!("not found or failed to fetch: {}", err_text),
            }))
        },
    }
}

pub async fn get_json(ctx: &AgentCtx, path: &str, pointer: Option<&str>) -> Result<Value, String> {
    let rel = normalize_rel_path(path)?;
    let key = join_storage_key(ctx, &rel);
    let bytes = match ctx.storage.get_bytes(&key).await {
        Ok(b) => b,
        Err(e) => {
            return Ok(
                serde_json::json!({"ok": false, "path": rel, "key": key, "error": format!("not found or failed to fetch: {}", e)}),
            )
        }
    };
    let v: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return Ok(
                serde_json::json!({"ok": false, "path": rel, "key": key, "error": format!("failed to parse json: {}", e)}),
            );
        }
    };
    if let Some(ptr) = pointer {
        let ptr = ptr.trim();
        if ptr.is_empty() {
            return Ok(serde_json::json!({"ok": true, "path": rel, "key": key, "json": v}));
        }
        if let Some(sub) = v.pointer(ptr) {
            return Ok(
                serde_json::json!({"ok": true, "path": rel, "key": key, "pointer": ptr, "json": sub}),
            );
        }
        return Ok(
            serde_json::json!({"ok": false, "path": rel, "key": key, "pointer": ptr, "error": "pointer not found"}),
        );
    }
    Ok(serde_json::json!({"ok": true, "path": rel, "key": key, "json": v}))
}

pub async fn manifest_find(
    ctx: &AgentCtx,
    path: &str,
    unique_id: Option<&str>,
    name: Option<&str>,
    resource_type: Option<&str>,
    limit: usize,
) -> Result<Value, String> {
    let rel = normalize_rel_path(path)?;
    let key = join_storage_key(ctx, &rel);
    let bytes = match ctx.storage.get_bytes(&key).await {
        Ok(b) => b,
        Err(e) => {
            return Ok(
                serde_json::json!({"ok": false, "path": rel, "key": key, "error": format!("not found or failed to fetch: {}", e)}),
            )
        }
    };
    let v: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return Ok(
                serde_json::json!({"ok": false, "path": rel, "key": key, "error": format!("failed to parse json: {}", e)}),
            );
        }
    };
    let nodes = match v.get("nodes").and_then(|n| n.as_object()) {
        Some(n) => n,
        None => {
            return Ok(
                serde_json::json!({"ok": false, "path": rel, "key": key, "error": "manifest missing nodes"}),
            )
        }
    };

    let mut out: Vec<serde_json::Value> = Vec::new();
    for (uid, node) in nodes.iter() {
        if let Some(ref u) = unique_id {
            if uid != u {
                continue;
            }
        }
        if let Some(ref n) = name {
            let node_name = node.get("name").and_then(|x| x.as_str()).unwrap_or("");
            if node_name != *n {
                continue;
            }
        }
        if let Some(ref rt) = resource_type {
            let node_rt = node
                .get("resource_type")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if node_rt != *rt {
                continue;
            }
        }
        let mut slim = serde_json::Map::new();
        slim.insert("unique_id".to_string(), serde_json::json!(uid));
        for k in [
            "resource_type",
            "name",
            "original_file_path",
            "path",
            "package_name",
            "database",
            "schema",
            "alias",
        ]
        .iter()
        {
            if let Some(vv) = node.get(*k) {
                slim.insert((*k).to_string(), vv.clone());
            }
        }
        if let Some(dep) = node.get("depends_on") {
            slim.insert("depends_on".to_string(), dep.clone());
        }
        out.push(Value::Object(slim));
        if out.len() >= limit {
            break;
        }
    }
    Ok(serde_json::json!({"ok": true, "path": rel, "key": key, "items": out}))
}

pub async fn remove_file(
    ctx: &AgentCtx,
    path: &str,
    expected_sha256: Option<&str>,
) -> Result<Value, String> {
    let rel = normalize_rel_path(path)?;
    let key = join_storage_key(ctx, &rel);

    let existing = ctx.storage.get_bytes(&key).await.ok();
    let existed = existing.is_some();
    let base_sha256 = existing
        .as_ref()
        .map(|b| sha256_hex(String::from_utf8_lossy(b).as_ref()))
        .unwrap_or_default();

    if let Some(expected) = expected_sha256 {
        if existed && expected != base_sha256 {
            return Err(format!(
                "expected_sha256 mismatch for {}: expected {}, current {}",
                rel, expected, base_sha256
            ));
        }
        // If the file doesn't exist, treat as idempotent no-op regardless of expected_sha256.
    }

    if existed {
        ctx.storage.delete_object(&key).await?;
    }

    Ok(serde_json::json!({
        "ok": true,
        "mutated": existed,
        "results": [{
            "op": "rm",
            "path": rel,
            "key": key,
            "existed": existed,
            "mutated": existed,
            "base_sha256": base_sha256
        }]
    }))
}

pub async fn move_file(
    ctx: &AgentCtx,
    from_path: &str,
    to_path: &str,
    expected_sha256: Option<&str>,
) -> Result<Value, String> {
    let from_rel = normalize_rel_path(from_path)?;
    let to_rel = normalize_rel_path(to_path)?;
    if from_rel == to_rel {
        return Err("mv requires from != to".to_string());
    }
    let from_key = join_storage_key(ctx, &from_rel);
    let to_key = join_storage_key(ctx, &to_rel);

    let bytes = ctx
        .storage
        .get_bytes(&from_key)
        .await
        .map_err(|_| format!("not found: {}", from_rel))?;
    let base_sha256 = sha256_hex(String::from_utf8_lossy(&bytes).as_ref());
    if let Some(expected) = expected_sha256 {
        if expected != base_sha256 {
            return Err(format!(
                "expected_sha256 mismatch for {}: expected {}, current {}",
                from_rel, expected, base_sha256
            ));
        }
    }

    // No implicit overwrite: destination must not exist.
    if ctx.storage.get_bytes(&to_key).await.is_ok() {
        return Err(format!("destination already exists: {}", to_rel));
    }

    ctx.storage
        .put_bytes(&to_key, &bytes, "text/plain")
        .await?;
    ctx.storage.delete_object(&from_key).await?;

    Ok(serde_json::json!({
        "ok": true,
        "mutated": true,
        "results": [{
            "op": "mv",
            "from": from_rel,
            "to": to_rel,
            "path": to_rel,
            "from_key": from_key,
            "key": to_key,
            "mutated": true,
            "base_sha256": base_sha256,
            "new_sha256": base_sha256
        }]
    }))
}

pub async fn apply_patch(
    ctx: &AgentCtx,
    datasets: Option<&std::sync::Arc<dyn DatasetCatalogProvider>>,
    path: &str,
    payload: &str,
    base_sha256: Option<&str>,
    expected_existed: Option<bool>,
    kind: PatchApplyKind,
) -> Result<PatchOutcome, String> {
    let rel = normalize_rel_path(path)?;
    let key = join_storage_key(ctx, &rel);
    let existing = ctx
        .storage
        .get_bytes(&key)
        .await
        .ok()
        .map(|b| String::from_utf8_lossy(&b).to_string());
    let existed = existing.is_some();
    let old = existing.unwrap_or_default();
    let base_hash = sha256_hex(&old);
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

            // Cursor/Aider hunks-only patches: no file headers; apply using flexible search/replace.
            if has_hunks && !has_file_headers {
                let normalized = normalize_hunks_only_patch_text(patch_text, &rel).map_err(|e| {
                    format!("invalid patch: {}", e)
                })?;
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
                if let Some(repl) = try_apply_unified_hunks_flexible(normalized.patch_text.as_str(), &old) {
                    apply_result_code = PatchApplyResultCode::AppliedHunksFlexible;
                    repl
                } else {
                    return Err("patch_hunk_context_miss: hunks-only patch could not be applied to current file content".to_string());
                }
            } else {
                let is_new_file_patch = patch_text.lines().any(|l| l.trim() == "--- /dev/null");
                // Safety guard: never allow "new file" semantics on an existing file. This commonly leads to
                // duplicated/concatenated content when LLMs attempt a full rewrite using a new-file diff.
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

                // Accept git-style patches (with optional preamble) by stripping down to the unified diff section
                // (`---`/`+++` + hunks) before feeding into diffy.
                let unified = strip_git_preamble_to_unified(patch_text)?;
            // diffy::Patch borrows from the patch text, so keep any repaired patch text alive
            // for the duration of parsing + apply.
            let mut patch_src: Cow<'_, str> = Cow::Borrowed(&unified);
            let mut parse_error_fallback_content: Option<String> = None;
            let patch = match Patch::from_str(patch_src.as_ref()) {
                Ok(p) => p,
                Err(e) => {
                    let emsg = e.to_string();
                    // Common LLM failure mode: incorrect hunk header counts or malformed hunk headers.
                    //
                    // diffy is strict and will reject mismatches with various error strings (including
                    // "Hunks not in order or overlap"). Attempt a deterministic repair by recomputing hunk
                    // counts and rewriting headers once, regardless of the specific parse error message.
                    let fixed = repair_unified_hunk_headers(&unified);
                    if fixed != unified {
                        patch_src = Cow::Owned(fixed);
                        apply_repairs.push("repaired_unified_hunk_headers".to_string());
                        match Patch::from_str(patch_src.as_ref()) {
                            Ok(p) => p,
                            Err(e2) => {
                                if let Some(repl) =
                                    try_apply_unified_hunks_flexible(patch_src.as_ref(), &old)
                                {
                                    apply_result_code = PatchApplyResultCode::AppliedUnifiedByFlexibleFallback;
                                    parse_error_fallback_content = Some(repl);
                                    Patch::from_str(
                                        "--- a/x\n+++ b/x\n@@ -1,0 +1,0 @@\n",
                                    )
                                    .map_err(|_| format!("invalid patch: {}", e2))?
                                } else {
                                    return Err(format!("invalid patch: {}", e2));
                                }
                            }
                        }
                    } else {
                        if let Some(repl) = try_apply_unified_hunks_flexible(&unified, &old) {
                            apply_result_code = PatchApplyResultCode::AppliedUnifiedByFlexibleFallback;
                            parse_error_fallback_content = Some(repl);
                            Patch::from_str("--- a/x\n+++ b/x\n@@ -1,0 +1,0 @@\n")
                                .map_err(|_| format!("invalid patch: {}", emsg))?
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
                            apply_result_code = PatchApplyResultCode::AppliedUnifiedAfterHeaderRepair;
                        } else {
                            apply_result_code = PatchApplyResultCode::AppliedUnifiedDirect;
                        }
                        c
                    }
                    Err(e) => {
                        // Fallbacks (in order):
                        // 1) full-file rewrite reconstruction from hunk body
                        // 2) flexible Cursor/Aider-style hunk search/replace
                        if let Some(repl) =
                            try_reconstruct_full_file_replacement(patch_src.as_ref(), &old)
                        {
                            apply_result_code =
                                PatchApplyResultCode::AppliedUnifiedByFullReplacementReconstruction;
                            repl
                        } else if let Some(repl) =
                            try_apply_unified_hunks_flexible(patch_src.as_ref(), &old)
                        {
                            apply_result_code = PatchApplyResultCode::AppliedUnifiedByFlexibleFallback;
                            repl
                        } else {
                            return Err(format!("patch apply failed: {}", e));
                        }
                    }
                }
            }
            }
        }
    };
    new_content = postprocess_content(ctx, datasets, &rel, &new_content).await?;

    let git_patch = create_git_patch_text(&old, &new_content, &rel, existed)?;
    let diff = compute_unified_diff(&old, &new_content);
    let (lines_added, lines_removed) = diff_stats(&old, &new_content);
    let new_hash = sha256_hex(&new_content);
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

/// If a unified diff patch is effectively "replace the whole file", extract the intended
/// resulting file contents directly from the patch hunk(s).
///
/// This is a fallback for strict diff application failures where the patch can't be applied
/// due to context mismatches, but the patch clearly contains the entire new file content.
fn try_reconstruct_full_file_replacement(unified: &str, old: &str) -> Option<String> {
    // Identify all hunk headers.
    let lines: Vec<&str> = unified.lines().collect();
    let mut hunk_idxs: Vec<usize> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if l.starts_with("@@") {
            hunk_idxs.push(i);
        }
    }
    if hunk_idxs.len() != 1 {
        return None;
    }
    let i = hunk_idxs[0];
    let header = lines[i];
    let after = header.trim_start_matches("@@").trim_start();
    let end_idx = after.find("@@")?;
    let ranges = after[..end_idx].trim();
    let parts: Vec<&str> = ranges.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    let (old_start, old_count) = parse_hunk_range(parts[0], '-')?;
    let (new_start, _new_count) = parse_hunk_range(parts[1], '+')?;

    // Conservative "whole file" check: hunk starts at (or before) first line, and claims to cover
    // at least the current file length. We use split('\n') to match other parts of this module.
    let old_lines_len = old.split('\n').count();
    if old_start > 1 || new_start > 1 {
        return None;
    }
    if old_count + 1 < old_lines_len {
        // +1 tolerance for trailing newline / last empty split segment.
        return None;
    }

    // Extract resulting lines from hunk body: keep ' ' and '+' lines, drop '-' lines.
    let mut out_lines: Vec<String> = Vec::new();
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
            '+' | ' ' => {
                // Strip the leading marker
                out_lines.push(l[1..].to_string());
            }
            '-' => {}
            _ => {}
        }
        j += 1;
    }
    Some(out_lines.join("\n"))
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

    // Aider-like permissive fallback: trim unchanged context and retry.
    let mut prefix = 0usize;
    let mut suffix = 0usize;
    while prefix < old_side.len()
        && prefix < new_side.len()
        && old_side[prefix] == new_side[prefix]
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
            match l.chars().next().unwrap_or(' ') {
                '-' => {
                    old_block.push(l[1..].to_string());
                    changed = true;
                }
                '+' => {
                    new_block.push(l[1..].to_string());
                    changed = true;
                }
                ' ' => {
                    old_block.push(l[1..].to_string());
                    new_block.push(l[1..].to_string());
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

        // Parse header like: @@ -0,0 +1,62 @@
        // Keep any suffix after the closing @@ (rare, but valid).
        let after = line.trim_start_matches("@@").trim_start();
        let Some(end_idx) = after.find("@@") else {
            out.push(line.to_string());
            i += 1;
            continue;
        };
        let ranges = after[..end_idx].trim();
        let suffix = &after[end_idx + 2..]; // may be empty
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

        // Count lines in the hunk body until next hunk header or EOF.
        let mut old_count = 0usize;
        let mut new_count = 0usize;
        let mut j = i + 1;
        while j < lines.len() {
            let l = lines[j];
            if l.starts_with("@@") {
                break;
            }
            if l.starts_with('\\') {
                // "\ No newline at end of file" — doesn't count toward either side.
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
    // Accept common git-style headers, but only keep the unified diff section (---/+++ + hunks),
    // which `diffy` can parse and apply.
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
        // Drop git metadata lines that can appear between diff --git and ---/+++ in some patches.
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

pub fn compute_unified_diff(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    out.push("--- original".to_string());
    out.push("+++ modified".to_string());
    let mut i = 0usize;
    let mut j = 0usize;
    while i < old_lines.len() || j < new_lines.len() {
        if i < old_lines.len() && j < new_lines.len() {
            if old_lines[i] == new_lines[j] {
                out.push(format!(" {}", old_lines[i]));
                i += 1;
                j += 1;
            } else {
                out.push(format!("- {}", old_lines[i]));
                out.push(format!("+ {}", new_lines[j]));
                i += 1;
                j += 1;
            }
        } else if i < old_lines.len() {
            out.push(format!("- {}", old_lines[i]));
            i += 1;
        } else {
            out.push(format!("+ {}", new_lines[j]));
            j += 1;
        }
    }
    out.join("\n")
}

pub fn diff_stats(old: &str, new: &str) -> (usize, usize) {
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut i = 0usize;
    let mut j = 0usize;
    while i < old_lines.len() || j < new_lines.len() {
        if i < old_lines.len() && j < new_lines.len() {
            if old_lines[i] == new_lines[j] {
                i += 1;
                j += 1;
            } else {
                removed += 1;
                added += 1;
                i += 1;
                j += 1;
            }
        } else if i < old_lines.len() {
            removed += 1;
            i += 1;
        } else {
            added += 1;
            j += 1;
        }
    }
    (added, removed)
}

fn is_allowed_rel_path(rel: &str) -> bool {
    let rel = rel.trim();
    if rel.is_empty() {
        return false;
    }
    if rel.starts_with('/') || rel.starts_with('\\') {
        return false;
    }
    if rel.contains("..") {
        return false;
    }
    // Allow any in-project file now that all writes go through patch/diff application
    // and are scoped to the configured dbt storage prefix.
    true
}

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let out = hasher.finalize();
    hex::encode(out)
}

fn is_missing_storage_error(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("nosuchkey")
        || e.contains("no such key")
        || e.contains("not found")
        || e.contains("404")
}

async fn postprocess_content(
    ctx: &AgentCtx,
    datasets: Option<&std::sync::Arc<dyn DatasetCatalogProvider>>,
    rel: &str,
    content: &str,
) -> Result<String, String> {
    if rel == project_files::PACKAGES_YML {
        return postprocess_packages_yml(content);
    }
    if rel == project_files::MODELS_SCHEMA_YML {
        return postprocess_schema_yml(ctx, datasets, content).await;
    }
    if rel.starts_with("models/staging/") && rel.ends_with(".yml") {
        return disable_contract_enforcement_in_schema_yml_text(content);
    }
    if rel.starts_with("models/") && rel.ends_with(".sql") {
        return postprocess_model_sql(ctx, rel, content);
    }
    Ok(content.to_string())
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

/// Canonicalize `models/schema.yml` content.
///
/// This is used when we want deterministic schema.yml normalization (e.g. rebuilding sources from
/// the dataset catalog) without requiring the caller to go through a patch-apply cycle.
pub async fn canonicalize_schema_yml(
    ctx: &AgentCtx,
    datasets: Option<&std::sync::Arc<dyn DatasetCatalogProvider>>,
    content: &str,
) -> Result<String, String> {
    postprocess_schema_yml(ctx, datasets, content).await
}

async fn postprocess_schema_yml(
    ctx: &AgentCtx,
    datasets: Option<&std::sync::Arc<dyn DatasetCatalogProvider>>,
    content: &str,
) -> Result<String, String> {
    // Dynamic, grounded sources:
    // - list_datasets is advisory only (can be incomplete due to permissions/caching).
    // - The only fact we trust is that QueryProvider.schema(<fqn>) succeeds.
    let q = ctx.warehouse.as_ref();
    let cfg = crate::config::resolved_config_from_ctx(ctx)
        .ok_or_else(|| "resolved_config missing for schema.yml postprocess".to_string())?;
    let want_catalog = cfg.providers.warehouse.container.clone();
    let want_schema = cfg.providers.warehouse.namespace.clone();
    const MAX_PROVED_SOURCES: usize = 200;

    fn parse_sources_from_schema_yml(root: &YamlMapping) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let Some(YamlValue::Sequence(srcs)) = root.get(&YamlValue::String("sources".to_string()))
        else {
            return out;
        };
        for src in srcs.iter() {
            let Some(m) = src.as_mapping() else { continue };
            let name = m
                .get(&YamlValue::String("name".to_string()))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let tables = m
                .get(&YamlValue::String("tables".to_string()))
                .and_then(|v| v.as_sequence())
                .cloned()
                .unwrap_or_default();
            for t in tables.into_iter() {
                let Some(tm) = t.as_mapping() else { continue };
                let tn = tm
                    .get(&YamlValue::String("name".to_string()))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !name.is_empty() && !tn.is_empty() {
                    out.push((name.clone(), tn));
                }
            }
        }
        out
    }

    fn sources_value_from_fqns(fqns: &std::collections::BTreeSet<String>) -> YamlValue {
        // Convert to the dbt schema.yml `sources:` structure:
        // - name: <schema>
        //   database: <catalog>
        //   schema: <schema>
        //   tables: [{ name: <table> }, ...]
        let grouped = crate::data_engineer::dataset_truth::group_by_catalog_schema(fqns);
        let mut sources_seq: Vec<YamlValue> = Vec::new();
        for ((cat, db), mut tables) in grouped.into_iter() {
            tables.sort();
            tables.dedup();
            let mut src = YamlMapping::new();
            src.insert(
                YamlValue::String("name".to_string()),
                YamlValue::String(db.clone()),
            );
            src.insert(
                YamlValue::String("database".to_string()),
                YamlValue::String(cat),
            );
            src.insert(
                YamlValue::String("schema".to_string()),
                YamlValue::String(db),
            );
            let mut tables_seq: Vec<YamlValue> = Vec::new();
            for t in tables.into_iter() {
                let mut tm = YamlMapping::new();
                tm.insert(YamlValue::String("name".to_string()), YamlValue::String(t));
                tables_seq.push(YamlValue::Mapping(tm));
            }
            src.insert(
                YamlValue::String("tables".to_string()),
                YamlValue::Sequence(tables_seq),
            );
            sources_seq.push(YamlValue::Mapping(src));
        }
        YamlValue::Sequence(sources_seq)
    }

    let mut root = if content.trim().is_empty() {
        YamlMapping::new()
    } else {
        let v: YamlValue =
            serde_yaml::from_str(content).map_err(|e| format!("schema.yml parse error: {}", e))?;
        match v {
            YamlValue::Mapping(m) => m,
            _ => return Err("models/schema.yml must be a YAML mapping at top level".to_string()),
        }
    };

    if !root.contains_key(&YamlValue::String("version".to_string())) {
        root.insert(
            YamlValue::String("version".to_string()),
            YamlValue::Number(2.into()),
        );
    }

    // Candidate sources:
    // - current schema.yml sources
    // - any source() calls found in staging SQL
    // - optional advisory: datasets.list_datasets() (bounded), but always re-checked via schema()
    let mut candidates: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // (A) Existing schema.yml sources
    for (schema, table) in parse_sources_from_schema_yml(&root).into_iter() {
        let s = schema.trim().to_string();
        let t = table.trim().to_string();
        if s == want_schema && !t.is_empty() {
            candidates.insert(format!("{}.{}.{}", want_catalog, s, t));
        }
    }

    // (B) Staging model SQL source() calls
    {
        let base = ctx
            .keyspace
            .dbt_prefix(&ctx.scope)
            .trim_end_matches('/')
            .to_string()
            + "/";
        let staging_prefix = format!("{}models/staging/", base);
        if let Ok(keys) = ctx.storage.list_prefix(&staging_prefix).await {
            for k in keys {
                if !k.ends_with(".sql") || k.contains("/_versions/") {
                    continue;
                }
                if let Ok(bytes) = ctx.storage.get_bytes(&k).await {
                    let sql = String::from_utf8_lossy(&bytes).to_string();
                    for (schema, table) in
                        crate::data_engineer::naming::extract_source_calls(&sql).into_iter()
                    {
                        if schema == want_schema && !table.trim().is_empty() {
                            candidates.insert(format!("{}.{}.{}", want_catalog, schema, table));
                        }
                    }
                }
            }
        }
    }

    // (C) Advisory discovery (optional, bounded)
    if let Some(ds) = datasets {
        if let Ok(listed) = ds.list_datasets().await {
            for d in listed.into_iter().take(MAX_PROVED_SOURCES) {
                if d.catalog == want_catalog && d.database == want_schema {
                    candidates.insert(d.fqn());
                }
            }
        }
    }

    // Prove candidates by schema() and only emit proven sources.
    let mut proven: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for fqn in candidates.into_iter() {
        if proven.len() >= MAX_PROVED_SOURCES {
            break;
        }
        if let Ok(_cols) = q.schema(&fqn).await {
            proven.insert(fqn);
        }
    }

    let sources_val = sources_value_from_fqns(&proven);
    root.insert(YamlValue::String("sources".to_string()), sources_val);
    serde_yaml::to_string(&YamlValue::Mapping(root)).map_err(|e| e.to_string())
}

fn postprocess_packages_yml(content: &str) -> Result<String, String> {
    let mut root = if content.trim().is_empty() {
        YamlMapping::new()
    } else {
        let v: YamlValue = serde_yaml::from_str(content)
            .map_err(|e| format!("packages.yml parse error: {}", e))?;
        match v {
            YamlValue::Mapping(m) => m,
            _ => return Err("packages.yml must be a YAML mapping at top level".to_string()),
        }
    };

    let packages_seq = match root.get(&YamlValue::String("packages".to_string())) {
        Some(YamlValue::Sequence(seq)) => seq.clone(),
        Some(_) => return Err("packages.yml 'packages' must be a list".to_string()),
        None => Vec::new(),
    };

    let mut by_key: BTreeMap<String, YamlValue> = BTreeMap::new();
    for item in packages_seq.into_iter() {
        let YamlValue::Mapping(m) = item else {
            continue;
        };
        let key = package_entry_key(&m).unwrap_or_else(|| format!("unknown:{}", by_key.len()));
        let canonical = canonicalize_package_entry(&m);
        by_key.insert(key, YamlValue::Mapping(canonical));
    }

    let normalized: Vec<YamlValue> = by_key.into_values().collect();
    root.insert(
        YamlValue::String("packages".to_string()),
        YamlValue::Sequence(normalized),
    );
    serde_yaml::to_string(&YamlValue::Mapping(root)).map_err(|e| e.to_string())
}

fn package_entry_key(m: &YamlMapping) -> Option<String> {
    let package = yaml_string_value(m, "package");
    let git = yaml_string_value(m, "git");
    let local = yaml_string_value(m, "local");
    if let Some(p) = package {
        return Some(format!("package:{}", p));
    }
    if let Some(g) = git {
        return Some(format!("git:{}", g));
    }
    if let Some(l) = local {
        return Some(format!("local:{}", l));
    }
    None
}

fn canonicalize_package_entry(m: &YamlMapping) -> YamlMapping {
    let mut out = YamlMapping::new();
    let package = yaml_string_value(m, "package");
    let git = yaml_string_value(m, "git");
    let local = yaml_string_value(m, "local");
    if let Some(p) = package {
        out.insert(
            YamlValue::String("package".to_string()),
            YamlValue::String(p),
        );
    } else if let Some(g) = git {
        out.insert(YamlValue::String("git".to_string()), YamlValue::String(g));
    } else if let Some(l) = local {
        out.insert(YamlValue::String("local".to_string()), YamlValue::String(l));
    }
    insert_if_present(&mut out, m, "version");
    insert_if_present(&mut out, m, "revision");
    insert_if_present(&mut out, m, "subdir");

    let mut extra: BTreeMap<String, YamlValue> = BTreeMap::new();
    for (k, v) in m {
        let Some(ks) = k.as_str() else { continue };
        if ks == "package"
            || ks == "git"
            || ks == "local"
            || ks == "version"
            || ks == "revision"
            || ks == "subdir"
        {
            continue;
        }
        extra.insert(ks.to_string(), v.clone());
    }
    for (k, v) in extra {
        out.insert(YamlValue::String(k), v);
    }
    out
}

fn insert_if_present(out: &mut YamlMapping, src: &YamlMapping, key: &str) {
    let key_val = YamlValue::String(key.to_string());
    if let Some(v) = src.get(&key_val) {
        out.insert(key_val, v.clone());
    }
}

fn yaml_string_value(m: &YamlMapping, key: &str) -> Option<String> {
    m.get(&YamlValue::String(key.to_string()))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn sources_value_from_dataset_ids(dss: &[DatasetId]) -> YamlValue {
    let mut by_cat_db: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for ds in dss {
        by_cat_db
            .entry((ds.catalog.clone(), ds.database.clone()))
            .or_default()
            .push(ds.table.clone());
    }
    let mut sources_seq: Vec<YamlValue> = Vec::new();
    for ((cat, db), mut tables) in by_cat_db.into_iter() {
        tables.sort();
        tables.dedup();
        let mut src = YamlMapping::new();
        src.insert(
            YamlValue::String("name".to_string()),
            YamlValue::String(db.clone()),
        );
        src.insert(
            YamlValue::String("database".to_string()),
            YamlValue::String(cat),
        );
        src.insert(
            YamlValue::String("schema".to_string()),
            YamlValue::String(db),
        );
        let mut tables_seq: Vec<YamlValue> = Vec::new();
        for t in tables.into_iter() {
            let mut tm = YamlMapping::new();
            tm.insert(YamlValue::String("name".to_string()), YamlValue::String(t));
            tables_seq.push(YamlValue::Mapping(tm));
        }
        src.insert(
            YamlValue::String("tables".to_string()),
            YamlValue::Sequence(tables_seq),
        );
        sources_seq.push(YamlValue::Mapping(src));
    }
    YamlValue::Sequence(sources_seq)
}

fn postprocess_model_sql(ctx: &AgentCtx, rel: &str, content: &str) -> Result<String, String> {
    validate_model_sql_identity(rel, content)?;
    let cfg = crate::config::resolved_config_from_ctx(ctx)
        .ok_or_else(|| "resolved_config missing for model SQL postprocess".to_string())?;
    let suffix = tier_suffix_for_path(rel, &cfg)
        .ok_or_else(|| "unable to infer tier suffix for model path".to_string())?;
    let alias = model_alias_from_rel(rel)
        .ok_or_else(|| "unable to infer model alias from path".to_string())?;
    Ok(rewrite_config_header(content, &suffix, &alias))
}

fn validate_model_sql_identity(rel: &str, content: &str) -> Result<(), String> {
    // Only validate dbt model SQL files.
    if !rel.starts_with("models/") || !rel.ends_with(".sql") {
        return Ok(());
    }

    // Silver: enforce strict 1:1 mapping between model file and source().
    if rel.starts_with("models/staging/") {
        let sources = naming::extract_source_calls(content);
        if sources.is_empty() {
            return Err(format!(
                "invalid silver model SQL at '{}': silver models under models/staging/ must contain exactly one dbt source() call and be written to the canonical path models/staging/stg_<source_schema>_<source_table>.sql",
                rel
            ));
        }
        if sources.len() != 1 {
            return Err(format!(
                "invalid silver model SQL at '{}': silver models under models/staging/ must reference exactly ONE source(schema, table). Found: {:?}",
                rel, sources
            ));
        }
        let (schema, table) = &sources[0];
        let canonical = naming::canonical_staging_rel_path(schema, table);
        if rel != canonical {
            return Err(format!(
                "invalid silver model path: silver model for source(\"{}\",\"{}\") must be written to '{}' (canonical), but attempted to write '{}'. Rename the file to the canonical path (no alternate naming schemes are permitted).",
                schema, table, canonical, rel
            ));
        }
        return Ok(());
    }

    // Gold/core/marts: must not read from raw/bronze sources.
    let sources = naming::extract_source_calls(content);
    if !sources.is_empty() {
        return Err(format!(
            "invalid gold/core model SQL at '{}': gold models must NOT reference dbt source() (raw/bronze). Use ref('stg_*') to read from silver. Found source() call(s): {:?}",
            rel, sources
        ));
    }
    Ok(())
}

fn tier_suffix_for_path(rel: &str, cfg: &crate::config::ReactResolvedConfig) -> Option<String> {
    if rel.starts_with("models/staging/") {
        return Some(cfg.providers.dbt.naming.silver_suffix.clone());
    }
    if rel.starts_with("models/core/") || rel.starts_with("models/marts/") {
        return Some(cfg.providers.dbt.naming.gold_suffix.clone());
    }
    if rel.starts_with("models/") {
        return Some(cfg.providers.dbt.naming.gold_suffix.clone());
    }
    None
}

fn model_alias_from_rel(rel: &str) -> Option<String> {
    let fname = Path::new(rel).file_name()?.to_string_lossy();
    let stem = fname.trim_end_matches(".sql");
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

fn rewrite_config_header(content: &str, _schema_suffix: &str, alias: &str) -> String {
    let mut body_lines: Vec<String> = Vec::new();
    for line in content.lines() {
        let t = line.trim();
        let is_config = (t.starts_with("{{") || t.starts_with("{%")) && t.contains("config(");
        if is_config {
            continue;
        }
        body_lines.push(line.to_string());
    }
    let body = body_lines.join("\n").trim_start().to_string();
    if body.is_empty() {
        return format!("{{{{ config(alias=\"{}\") }}}}\n", alias);
    }
    format!("{{{{ config(alias=\"{}\") }}}}\n\n{}", alias, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::{ChatMessage, LargeLanguageModel};
    use react_core::providers::{QueryProvider, QueryResult};
    use react_core::scope::RequestScope;
    use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};
    use std::collections::HashMap;
    use std::sync::Arc;

    #[derive(Default)]
    struct DummyLlm;

    impl LargeLanguageModel for DummyLlm {
        fn chat(
            &self,
            _messages: &[ChatMessage],
            _options: &react_core::llm::LlmCallOptions,
        ) -> Result<String, String> {
            Err("not used".to_string())
        }
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
    }

    #[derive(Clone)]
    struct MockDatasets {
        items: Vec<DatasetId>,
    }

    #[async_trait]
    impl DatasetCatalogProvider for MockDatasets {
        async fn list_datasets(&self) -> Result<Vec<DatasetId>, String> {
            Ok(self.items.clone())
        }
        async fn get_dataset_schema(
            &self,
            _dataset: &DatasetId,
        ) -> Result<Vec<(String, String)>, String> {
            Ok(vec![])
        }
        async fn get_dataset_stats(
            &self,
            _dataset: &DatasetId,
            _max_fields: usize,
        ) -> Result<
            (
                react_core::discover::stats::DatasetFieldStats,
                react_core::providers::catalog::types::DatasetStats,
            ),
            String,
        > {
            Err("not implemented".to_string())
        }
    }

    fn minimal_cfg() -> Arc<crate::config::ReactResolvedConfig> {
        Arc::new(crate::config::ReactResolvedConfig {
            server: crate::config::ServerResolved { port: 1 },
            storage: crate::config::StorageResolved {
                bucket: "b".to_string(),
            },
            scope: RequestScope {
                tenant: "t".to_string(),
                workspace: "w".to_string(),
                project_id: "p".to_string(),
            },
            llm: crate::config::LlmResolved::default(),
            providers: crate::config::ProvidersResolved {
                warehouse: crate::config::WarehouseResolved {
                    kind: "athena".to_string(),
                    container: "AwsDataCatalog".to_string(),
                    namespace: "test_raw".to_string(),
                    extras: serde_json::json!({"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"}),
                },
                catalog: crate::config::CatalogResolved {
                    enabled: false,
                    refresh_secs: 60,
                    max_concurrency: 8,
                },
                dbt: crate::config::DbtResolved {
                    enabled: true,
                    profiles_dir: None,
                    target: "athena".to_string(),
                    naming: crate::config::DbtNamingResolved {
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
                vector: crate::config::VectorResolved { enabled: false },
            },
        })
    }

    #[derive(Clone, Default)]
    struct MockQuery {
        schemas: Arc<std::sync::Mutex<HashMap<String, Vec<(String, String)>>>>,
    }

    #[async_trait]
    impl QueryProvider for MockQuery {
        async fn query(&self, _sql: &str) -> Result<QueryResult, String> {
            Err("not implemented".to_string())
        }
        async fn schema(&self, dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
            let m = self.schemas.lock().unwrap();
            m.get(dataset_fqn)
                .cloned()
                .ok_or_else(|| format!("not found: {}", dataset_fqn))
        }
        async fn sample(
            &self,
            _dataset_fqn: &str,
            _limit: usize,
        ) -> Result<Vec<Vec<String>>, String> {
            Err("not implemented".to_string())
        }
    }

    #[derive(Clone, Default)]
    struct MockWarehouse {
        schemas: Arc<std::sync::Mutex<HashMap<String, Vec<(String, String)>>>>,
    }

    #[async_trait]
    impl QueryProvider for MockWarehouse {
        async fn query(&self, _sql: &str) -> Result<QueryResult, String> {
            Err("not implemented".to_string())
        }
        async fn schema(&self, dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
            let m = self.schemas.lock().unwrap();
            m.get(dataset_fqn)
                .cloned()
                .ok_or_else(|| format!("not found: {}", dataset_fqn))
        }
        async fn sample(
            &self,
            _dataset_fqn: &str,
            _limit: usize,
        ) -> Result<Vec<Vec<String>>, String> {
            Err("not implemented".to_string())
        }
        fn max_concurrency(&self) -> usize {
            1
        }
    }

    #[async_trait]
    impl DatasetCatalogProvider for MockWarehouse {
        async fn list_datasets(&self) -> Result<Vec<DatasetId>, String> {
            Ok(vec![])
        }
        async fn get_dataset_schema(
            &self,
            dataset: &DatasetId,
        ) -> Result<Vec<(String, String)>, String> {
            self.schema(&dataset.fqn()).await
        }
        async fn get_dataset_stats(
            &self,
            _dataset: &DatasetId,
            _max_fields: usize,
        ) -> Result<
            (
                react_core::discover::stats::DatasetFieldStats,
                react_core::providers::catalog::types::DatasetStats,
            ),
            String,
        > {
            Err("not implemented".to_string())
        }
    }

    impl react_core::providers::WarehouseNaming for MockWarehouse {
        fn kind(&self) -> &'static str {
            "mock"
        }
        fn parse_dataset_fqn(&self, dataset_fqn: &str) -> Result<DatasetId, String> {
            let raw = dataset_fqn.trim().trim_matches('"').trim_matches('`');
            let parts: Vec<&str> = raw.split('.').collect();
            if parts.len() != 3 {
                return Err("mock dataset id must be <catalog>.<schema>.<table>".to_string());
            }
            Ok(DatasetId {
                catalog: parts[0].to_string(),
                database: parts[1].to_string(),
                table: parts[2].to_string(),
            })
        }
        fn quote_ident(&self, ident: &str) -> String {
            format!("\"{}\"", ident.replace('"', "\"\""))
        }
    }

    fn make_ctx(
        storage: Arc<dyn StorageAdapter>,
        query: Option<Arc<dyn QueryProvider>>,
    ) -> AgentCtx {
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
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm: Arc::new(DummyLlm::default()),
            storage,
            scope: scope.clone(),
            keyspace,
            query,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            runtime: Some(minimal_cfg() as Arc<dyn std::any::Any + Send + Sync>),
        }
    }

    #[tokio::test]
    async fn apply_patch_creates_file_and_injects_config() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
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
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
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
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
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
        assert!(err.contains("exactly ONE source"));
    }

    #[tokio::test]
    async fn apply_patch_rejects_gold_model_using_source() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
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
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let q = MockQuery::default();
        *q.schemas.lock().unwrap() = HashMap::from([
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
        let datasets: Arc<dyn DatasetCatalogProvider> = Arc::new(MockDatasets {
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

    #[tokio::test]
    async fn schema_yml_filters_unproven_sources_via_schema_facts() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let q = MockQuery::default();
        *q.schemas.lock().unwrap() = HashMap::from([(
            "AwsDataCatalog.test_raw.raw_customers".to_string(),
            vec![("id".to_string(), "varchar".to_string())],
        )]);
        let warehouse: Arc<dyn react_core::providers::WarehouseProvider> =
            Arc::new(MockWarehouse {
                schemas: q.schemas.clone(),
            });
        let ctx = AgentCtx {
            warehouse,
            ..make_ctx(storage, Some(Arc::new(q)))
        };
        let datasets: Arc<dyn DatasetCatalogProvider> = Arc::new(MockDatasets { items: vec![] });

        let existing = r#"
version: 2
sources:
  - name: test_raw
    database: AwsDataCatalog
    schema: test_raw
    tables:
      - name: raw_customers
      - name: raw_products
"#;
        let out = canonicalize_schema_yml(&ctx, Some(&datasets), existing)
            .await
            .expect("ok");
        assert!(out.contains("raw_customers"));
        assert!(!out.contains("raw_products"));
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
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage, None);

        // Note: the hunk header intentionally lies about how many lines are added.
        // `apply_patch` should repair the header counts and apply successfully.
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
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
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
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
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
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage.clone(), None);
        let rel = "macros/helpers.sql";
        let key = join_storage_key(&ctx, rel);
        ctx.storage
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
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage.clone(), None);
        let rel = "macros/helpers.sql";
        let key = join_storage_key(&ctx, rel);
        let seed = "fn x() {\n    if ok {\n        return 1;\n    }\n}\n";
        ctx.storage
            .put_bytes(&key, seed.as_bytes(), "text/sql")
            .await
            .expect("seed");
        // Hunk body is outdented (common LLM defect). Flexible matcher should still apply.
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
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
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
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage.clone(), None);
        let rel = "models/core/x.sql";
        let key = join_storage_key(&ctx, rel);
        ctx.storage
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
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage.clone(), None);
        let rel = "models/core/x.sql";
        let key = join_storage_key(&ctx, rel);
        ctx.storage
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
        assert_eq!(out.apply_result_code, PatchApplyResultCode::AppliedHunksFlexible);
        assert!(out
            .apply_repairs
            .iter()
            .any(|r| r.starts_with("normalized_line_number_hunk_headers=")));
    }
}
