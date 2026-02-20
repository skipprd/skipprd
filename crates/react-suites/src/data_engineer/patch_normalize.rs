use serde_json::{Map, Value};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchErrorKind {
    Structural,
    Contract,
}

fn err(kind: PatchErrorKind, msg: impl AsRef<str>) -> String {
    let k = match kind {
        PatchErrorKind::Structural => "PATCH_STRUCTURAL",
        PatchErrorKind::Contract => "PATCH_CONTRACT",
    };
    format!("{k}: {}", msg.as_ref().trim())
}

fn take_string(obj: &mut Map<String, Value>, key: &str) -> Option<String> {
    obj.remove(key)
        .and_then(|v| v.as_str().map(|s| s.to_string()))
}

fn normalize_expected_sha256(obj: &mut Map<String, Value>) {
    // Backward compatibility: map base_sha256 -> expected_sha256 if expected_sha256 missing/empty.
    let base = take_string(obj, "base_sha256")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if base.is_none() {
        return;
    }
    let needs_expected = obj
        .get("expected_sha256")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().is_empty())
        .unwrap_or(true);
    if needs_expected {
        obj.insert(
            "expected_sha256".to_string(),
            Value::String(base.unwrap()),
        );
    }
}

fn normalize_new_text(obj: &mut Map<String, Value>) -> Result<(), String> {
    if obj.get("new_text").and_then(|v| v.as_str()).is_some() {
        return Ok(());
    }
    // Common LLM field drift.
    for k in ["newText", "content", "contents", "text", "new_contents", "newContent"] {
        if let Some(v) = obj.remove(k) {
            if let Some(s) = v.as_str() {
                let t = s.to_string();
                obj.insert("new_text".to_string(), Value::String(t));
                return Ok(());
            }
            return Err(err(
                PatchErrorKind::Contract,
                format!("{k} must be a string"),
            ));
        }
    }
    Err(err(
        PatchErrorKind::Contract,
        "missing required field `new_text` (and no known alias field present)",
    ))
}

fn normalize_edit_new_text(obj: &mut Map<String, Value>) -> Result<(), String> {
    if obj.get("new_text").and_then(|v| v.as_str()).is_some() {
        return Ok(());
    }
    for k in ["newText", "content", "contents", "text", "new_contents", "newContent"] {
        if let Some(v) = obj.remove(k) {
            if let Some(s) = v.as_str() {
                obj.insert("new_text".to_string(), Value::String(s.to_string()));
                return Ok(());
            }
            return Err(err(
                PatchErrorKind::Contract,
                format!("{k} must be a string"),
            ));
        }
    }
    Err(err(
        PatchErrorKind::Contract,
        "missing required field `new_text` in edits item",
    ))
}

fn normalize_path(
    obj: &mut Map<String, Value>,
    fallback_path: Option<&str>,
    expected_rel_path: Option<&str>,
) -> Result<(), String> {
    let existing = obj
        .get("path")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if existing.is_some() {
        return Ok(());
    }
    let fill = expected_rel_path
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .or_else(|| fallback_path.map(|s| s.trim()).filter(|s| !s.is_empty()));
    if let Some(p) = fill {
        obj.insert("path".to_string(), Value::String(p.to_string()));
        return Ok(());
    }
    Err(err(
        PatchErrorKind::Contract,
        "missing required field `path` (no expected_rel_path or top-level path provided)",
    ))
}

fn reject_unknown_fields(
    obj: &Map<String, Value>,
    allowed: &[&str],
) -> Result<(), String> {
    let allow: HashSet<&str> = allowed.iter().copied().collect();
    let mut unknown: Vec<String> = obj
        .keys()
        .filter(|k| !allow.contains(k.as_str()))
        .cloned()
        .collect();
    unknown.sort();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(err(
        PatchErrorKind::Contract,
        format!("unknown field(s): {}", unknown.join(", ")),
    ))
}

fn normalize_primitive_obj(
    v: &mut Value,
    primitive_key: &str,
    fallback_path: Option<&str>,
    expected_rel_path: Option<&str>,
) -> Result<(), String> {
    let Some(obj) = v.as_object_mut() else {
        return Err(err(
            PatchErrorKind::Contract,
            format!("{primitive_key} must be an object"),
        ));
    };
    normalize_expected_sha256(obj);
    normalize_path(obj, fallback_path, expected_rel_path)?;

    match primitive_key {
        "replace_file" => {
            normalize_new_text(obj)?;
            reject_unknown_fields(obj, &["path", "new_text", "expected_sha256"])
        }
        "replace_range" => {
            normalize_new_text(obj)?;
            reject_unknown_fields(
                obj,
                &["path", "start_line", "end_line", "new_text", "expected_sha256"],
            )
        }
        "replace_list" => {
            let edits = obj
                .get_mut("edits")
                .ok_or_else(|| err(PatchErrorKind::Contract, "replace_list missing required field `edits`"))?;
            let Some(arr) = edits.as_array_mut() else {
                return Err(err(PatchErrorKind::Contract, "replace_list.edits must be an array"));
            };
            for it in arr.iter_mut() {
                let Some(eobj) = it.as_object_mut() else {
                    return Err(err(PatchErrorKind::Contract, "replace_list.edits items must be objects"));
                };
                normalize_edit_new_text(eobj)?;
                reject_unknown_fields(eobj, &["start_line", "end_line", "new_text"])?;
            }
            reject_unknown_fields(obj, &["path", "edits", "expected_sha256"])
        }
        _ => Ok(()),
    }?;
    Ok(())
}

/// Normalize `dbt_files op=patch` args into a strict, predictable canonical shape.
///
/// Fixups (deterministic):
/// - Map `base_sha256` -> `expected_sha256`
/// - Fill missing `path` from top-level `args.path`
/// - Map common text aliases (`content`/`contents`/`text`/`newText`) -> `new_text`
/// - Convert `replace_file: \"...\"` into `{path, new_text}` when possible
///
/// Errors are tagged as `PATCH_STRUCTURAL:` or `PATCH_CONTRACT:` for downstream classification.
pub fn normalize_dbt_files_patch_args(args: &Value) -> Result<Value, String> {
    fn contains_key_recursive(v: &Value, key: &str) -> bool {
        match v {
            Value::Object(m) => m.contains_key(key) || m.values().any(|vv| contains_key_recursive(vv, key)),
            Value::Array(a) => a.iter().any(|vv| contains_key_recursive(vv, key)),
            _ => false,
        }
    }

    if contains_key_recursive(args, "preview_diff") {
        return Err(err(
            PatchErrorKind::Contract,
            "preview_diff is no longer supported; remove it from the request",
        ));
    }

    let mut root = args
        .as_object()
        .cloned()
        .ok_or_else(|| err(PatchErrorKind::Contract, "args must be a JSON object"))?;
    let fallback_path = root
        .get("path")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    for key in ["replace_file", "replace_range", "replace_list"] {
        let Some(v) = root.get_mut(key) else { continue };
        if v.is_null() {
            continue;
        }

        // Special-case: LLM sometimes emits replace_file as a raw string containing new_text.
        if key == "replace_file" && v.is_string() {
            let Some(txt) = v.as_str().map(|s| s.to_string()) else {
                return Err(err(
                    PatchErrorKind::Contract,
                    "replace_file must be an object or array (got non-string scalar)",
                ));
            };
            let p = fallback_path
                .as_deref()
                .ok_or_else(|| err(PatchErrorKind::Contract, "replace_file was a string but no path was provided"))?;
            *v = Value::Object(Map::from_iter([
                ("path".to_string(), Value::String(p.to_string())),
                ("new_text".to_string(), Value::String(txt)),
            ]));
        }

        if v.is_array() {
            let arr = v.as_array_mut().unwrap();
            for it in arr.iter_mut() {
                normalize_primitive_obj(
                    it,
                    key,
                    fallback_path.as_deref(),
                    None,
                )?;
            }
            // Canonicalize: collapse single-element arrays.
            if arr.len() == 1 {
                let one = arr.pop().unwrap();
                *v = one;
            }
        } else {
            normalize_primitive_obj(v, key, fallback_path.as_deref(), None)?;
        }
    }

    Ok(Value::Object(root))
}

/// Normalize a single-file patch response (patch_protocol) in a strict, canonical way.
///
/// - Enforces `expected_rel_path` for missing/empty path fields
/// - Collapses single-element arrays to objects
/// - Rejects multi-element arrays (structural)
pub fn normalize_single_file_patch_response_value(
    mut v: Value,
    expected_rel_path: &str,
) -> Result<Value, String> {
    let Some(obj) = v.as_object_mut() else {
        return Err(err(PatchErrorKind::Contract, "patch response must be a JSON object"));
    };

    for key in ["replace_file", "replace_range", "replace_list"] {
        let Some(entry) = obj.get_mut(key) else { continue };
        if entry.is_null() {
            continue;
        }
        if entry.is_array() {
            let arr = entry.as_array_mut().unwrap();
            if arr.len() != 1 {
                return Err(err(
                    PatchErrorKind::Structural,
                    format!("{key} expected a single object or single-element array"),
                ));
            }
            let one = arr.pop().unwrap();
            *entry = one;
        }
        if key == "replace_file" && entry.is_string() {
            let txt = entry.as_str().unwrap().to_string();
            *entry = Value::Object(Map::from_iter([
                ("path".to_string(), Value::String(expected_rel_path.to_string())),
                ("new_text".to_string(), Value::String(txt)),
            ]));
        }
        normalize_primitive_obj(entry, key, None, Some(expected_rel_path))?;
    }

    Ok(v)
}

