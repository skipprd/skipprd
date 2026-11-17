//! `transform.compute` `type: py` is a file + handler. The host never execs
//! source text; it loads that file in a bounded `python3` worker.

use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::helpers::configuration::{ComputeItem, Config};
use crate::pipeline_type::COPY_WAL_FINGERPRINT;

use super::Ident;

const WORKER: &str = include_str!("py_worker.py");
const PYTHON3: &str = "python3";

/// Max bytes of one sibling `.py` file hashed with the handler.
pub const PY_FILE_MAX_BYTES: usize = 1_048_576;
const PY_DIR_MAX_FILES: usize = 64;
const PY_DIR_MAX_BYTES: usize = 8 * 1024 * 1024;
const PY_STDIN_MAX_BYTES: usize = 64 * 1024 * 1024;
const PY_BATCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Arrow ingest without flatten cannot feed JSON objects to `type: py`.
pub const PY_UNFLATTENED_ARROW: &str =
    "python compute requires JSON objects; Arrow ingest without flatten_events cannot run type: py";

/// `type: py` maps JSON objects. Unflattened Arrow batches have no objects.
pub fn blocks_unflattened_arrow(has_py: bool, flatten: bool) -> bool {
    has_py && !flatten
}

/// Resolved `type: py` item: authored path, canonical file, handler, sibling digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PyLoad {
    pub name: String,
    pub path: String,
    pub abs_path: PathBuf,
    pub handler: String,
    pub digest: String,
}

#[derive(Debug, Deserialize)]
struct WorkerOut {
    results: Vec<WorkerResult>,
}

#[derive(Debug, Deserialize)]
struct WorkerResult {
    #[serde(default)]
    record: Option<Value>,
    #[serde(default)]
    error: Option<String>,
}

pub fn has_py(config: &Config) -> bool {
    config
        .get_transform_compute()
        .iter()
        .any(ComputeItem::is_py)
}

pub fn py_loads(config: &Config) -> Result<Vec<PyLoad>, String> {
    let items = config.get_transform_compute();
    if !items.iter().any(ComputeItem::is_py) {
        return Ok(Vec::new());
    }
    let dir = config
        .config_dir
        .as_deref()
        .ok_or_else(|| "python compute path requires skippr.yml".to_string())?;
    let mut loads = Vec::new();
    for item in items {
        let ComputeItem::Py {
            name,
            path,
            handler,
        } = item
        else {
            continue;
        };
        let mut load = admit_item(dir, &path, &handler)?;
        load.name = name;
        loads.push(load);
    }
    Ok(loads)
}

/// Disk path, handler, and digest must match journaled `ComputeKind::Py`.
/// Path retarget or sibling edits fail closed until DROP then CREATE.
pub fn assert_py_matches_journal(
    pipeline: &str,
    loads: &[PyLoad],
    log: &super::ComputeLog,
) -> Result<(), String> {
    for load in loads {
        let fqn = super::Fqn::parse_item(pipeline, &load.name)?;
        let Some(entry) = log.get(&fqn) else {
            return Err(format!("python compute {fqn} is not in the journal"));
        };
        match &entry.kind {
            super::ComputeKind::Py {
                path,
                handler,
                digest,
                ..
            } if path == &load.path && handler == &load.handler && digest == &load.digest => {}
            super::ComputeKind::Py { .. } => {
                return Err(py_changed_message(&fqn));
            }
            _ => return Err(format!("compute {fqn} is not type: py")),
        }
    }
    Ok(())
}

pub fn admit_item(config_dir: &Path, path: &str, handler: &str) -> Result<PyLoad, String> {
    admit_handler(handler)?;
    let abs_path = resolve_entry(config_dir, path)?;
    let parent = abs_path.parent().ok_or_else(|| {
        format!(
            "python compute path {} has no directory",
            abs_path.display()
        )
    })?;
    let digest = dir_digest(parent, handler)?;
    Ok(PyLoad {
        name: String::new(),
        path: path.to_string(),
        abs_path,
        handler: handler.to_string(),
        digest,
    })
}

/// Py identity change is DROP then CREATE. Not `ALTER COMPUTE`.
pub fn py_changed_message(name: &super::Fqn) -> String {
    format!("compute {name} changed; DROP and CREATE to migrate")
}

/// Copy WAL identity when python compute was not admitted. Not `"copy"`.
pub const COPY_PY_UNADMITTED: &str = "copy#python-unadmitted";

/// Copy WAL fingerprint. Unchanged `"copy"` when there is no py compute.
pub fn copy_wal_fingerprint(loads: &[PyLoad]) -> String {
    if loads.is_empty() {
        return COPY_WAL_FINGERPRINT.to_string();
    }
    let mut bytes = Vec::new();
    for load in loads {
        bytes.extend_from_slice(load.name.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(load.path.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(load.handler.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(load.digest.as_bytes());
        bytes.push(0);
    }
    format!("copy#{:x}", md5::compute(bytes))
}

/// Map JSON objects through each `type: py` load in config order. 1:1.
/// Worker crash / timeout fails the whole batch. Per-record handler errors
/// are `Err` slots so the caller can deadletter that row.
pub fn apply_py_records(
    loads: &[PyLoad],
    records: Vec<Value>,
) -> Result<Vec<Result<Value, String>>, String> {
    if loads.is_empty() {
        return Ok(records.into_iter().map(Ok).collect());
    }
    let mut current: Vec<Result<Value, String>> = records.into_iter().map(Ok).collect();
    for load in loads {
        let mut batch = Vec::new();
        let mut idx = Vec::new();
        for (i, slot) in current.iter().enumerate() {
            if let Ok(value) = slot {
                idx.push(i);
                batch.push(value.clone());
            }
        }
        if batch.is_empty() {
            continue;
        }
        let outs = run_worker(load, &batch)?;
        if outs.len() != batch.len() {
            return Err(format!(
                "python compute returned {} results for {} records",
                outs.len(),
                batch.len()
            ));
        }
        for (j, out) in outs.into_iter().enumerate() {
            current[idx[j]] = out;
        }
    }
    Ok(current)
}

fn admit_handler(handler: &str) -> Result<(), String> {
    Ident::new(handler).map_err(|err| format!("python compute handler: {err}"))?;
    let mut chars = handler.chars();
    let Some(first) = chars.next() else {
        return Err("python compute handler must be a Python identifier".into());
    };
    if !(first.is_ascii_alphabetic() || first == '_')
        || !handler
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err("python compute handler must be a Python identifier".into());
    }
    Ok(())
}

fn resolve_entry(config_dir: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.trim().is_empty() {
        return Err("python compute path must be non-empty".into());
    }
    let path = Path::new(rel);
    if path.is_absolute() {
        return Err("python compute path must be relative to skippr.yml".into());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err("python compute path cannot contain ..".into());
            }
        }
    }
    if path.extension() != Some(OsStr::new("py")) {
        return Err("python compute path must end with .py".into());
    }
    let joined = config_dir.join(path);
    let abs = joined
        .canonicalize()
        .map_err(|err| format!("python compute path {}: {err}", joined.display()))?;
    let root = config_dir
        .canonicalize()
        .map_err(|err| format!("python compute skippr.yml directory: {err}"))?;
    if abs.strip_prefix(&root).is_err() {
        return Err("python compute path escapes skippr.yml directory".into());
    }
    if !abs.is_file() {
        return Err(format!(
            "python compute path {} is not a file",
            abs.display()
        ));
    }
    Ok(abs)
}

fn dir_digest(dir: &Path, handler: &str) -> Result<String, String> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).map_err(|err| format!("python compute directory: {err}"))? {
        let entry = entry.map_err(|err| format!("python compute directory: {err}"))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension() != Some(OsStr::new("py")) {
            continue;
        }
        files.push(path);
    }
    files.sort();
    if files.len() > PY_DIR_MAX_FILES {
        return Err(format!(
            "python compute directory exceeds {PY_DIR_MAX_FILES} .py files"
        ));
    }
    let mut total = 0usize;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(handler.as_bytes());
    bytes.push(0);
    for path in &files {
        let name = path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| "python compute file name is not utf-8".to_string())?;
        let data = fs::read(path).map_err(|err| format!("python compute read {name}: {err}"))?;
        if data.len() > PY_FILE_MAX_BYTES {
            return Err(format!(
                "python compute file {name} exceeds {PY_FILE_MAX_BYTES} bytes"
            ));
        }
        total = total.saturating_add(data.len());
        if total > PY_DIR_MAX_BYTES {
            return Err(format!(
                "python compute directory exceeds {PY_DIR_MAX_BYTES} bytes"
            ));
        }
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&data);
        bytes.push(0);
    }
    Ok(format!("{:x}", md5::compute(bytes)))
}

fn run_worker(load: &PyLoad, records: &[Value]) -> Result<Vec<Result<Value, String>>, String> {
    let payload = serde_json::to_vec(&json!({
        "path": load.abs_path,
        "handler": load.handler,
        "records": records,
    }))
    .map_err(|err| format!("python compute stdin: {err}"))?;
    if payload.len() > PY_STDIN_MAX_BYTES {
        return Err(format!(
            "python compute stdin exceeds {PY_STDIN_MAX_BYTES} bytes"
        ));
    }
    let mut child = Command::new(PYTHON3)
        .arg("-c")
        .arg(WORKER)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("python3: {err}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "python3 stdin".to_string())?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "python3 stdout".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "python3 stderr".to_string())?;
    let writer = std::thread::spawn(move || stdin.write_all(&payload).and_then(|_| stdin.flush()));
    let stdout_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).map(|_| buf)
    });
    let stderr_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).map(|_| buf)
    });
    let status = wait_child(&mut child)?;
    let _ = writer.join();
    let stdout_buf = stdout_t
        .join()
        .map_err(|_| "python3 stdout join".to_string())?
        .map_err(|err| format!("python3 stdout: {err}"))?;
    let stderr_buf = stderr_t
        .join()
        .map_err(|_| "python3 stderr join".to_string())?
        .map_err(|err| format!("python3 stderr: {err}"))?;
    let stderr_text = String::from_utf8_lossy(&stderr_buf);
    if stdout_buf.len() > PY_STDIN_MAX_BYTES {
        return Err(format!(
            "python compute stdout exceeds {PY_STDIN_MAX_BYTES} bytes"
        ));
    }
    if !status.success() {
        let tail = stderr_text.trim();
        if tail.is_empty() {
            return Err(format!("python3 exited {}", status));
        }
        return Err(tail.to_string());
    }
    let out: WorkerOut = serde_json::from_slice(&stdout_buf).map_err(|err| {
        format!(
            "python compute stdout is not results JSON: {err}: {}",
            String::from_utf8_lossy(&stdout_buf)
        )
    })?;
    if out.results.len() != records.len() {
        return Err(format!(
            "python compute returned {} results for {} records",
            out.results.len(),
            records.len()
        ));
    }
    out.results
        .into_iter()
        .map(|row| match (row.record, row.error) {
            (Some(value), None) if value.is_object() => Ok(Ok(value)),
            (None, Some(err)) => Ok(Err(err)),
            (Some(_), None) => Err("python compute record is not a JSON object".into()),
            (Some(_), Some(_)) => Err("python compute result has record and error".into()),
            (None, None) => Err("python compute result is empty".into()),
        })
        .collect()
}

fn wait_child(child: &mut std::process::Child) -> Result<std::process::ExitStatus, String> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if start.elapsed() >= PY_BATCH_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "python compute exceeded {}s",
                    PY_BATCH_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(err) => return Err(format!("python3 wait: {err}")),
        }
    }
}

#[cfg(test)]
pub(crate) fn write_py_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in files {
        std::fs::write(dir.path().join(name), body.as_bytes()).unwrap();
    }
    dir
}

#[cfg(test)]
fn load_named(dir: &Path, rel: &str, handler: &str, name: &str) -> PyLoad {
    let mut load = admit_item(dir, rel, handler).unwrap();
    load.name = name.to_string();
    load
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn value_is_unknown_field() {
        let err = serde_yaml::from_str::<ComputeItem>(
            "type: py\nname: enrich\nvalue: \"def transform(record): return record\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn unflattened_arrow_blocks_py() {
        assert!(blocks_unflattened_arrow(true, false));
        assert!(!blocks_unflattened_arrow(true, true));
        assert!(!blocks_unflattened_arrow(false, false));
        assert_eq!(
            PY_UNFLATTENED_ARROW,
            "python compute requires JSON objects; Arrow ingest without flatten_events cannot run type: py"
        );
    }

    #[test]
    fn dir_digest_rejects_too_many_files() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..=PY_DIR_MAX_FILES {
            std::fs::write(dir.path().join(format!("f{i}.py")), b"x=1\n").unwrap();
        }
        let err = dir_digest(dir.path(), "transform").unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn admit_handler_rejects_empty_and_dotted() {
        assert!(admit_handler("").is_err());
        assert!(admit_handler("enrich.transform").is_err());
        assert!(admit_handler("foo-bar").is_err());
        assert!(admit_handler("transform").is_ok());
    }

    #[test]
    fn path_must_be_relative_py_under_config_dir() {
        let dir = write_py_dir(&[("stamp.py", "def transform(record):\n    return record\n")]);
        assert!(resolve_entry(dir.path(), "/tmp/stamp.py").is_err());
        assert!(resolve_entry(dir.path(), "../stamp.py").is_err());
        assert!(resolve_entry(dir.path(), "stamp.txt").is_err());
        assert!(resolve_entry(dir.path(), "missing.py").is_err());
        assert!(resolve_entry(dir.path(), "stamp.py").is_ok());
    }

    #[test]
    fn path_symlink_outside_config_dir_fails() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("stamp.py"),
            b"def transform(record):\n    return record\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path().join("stamp.py"), dir.path().join("link.py"))
            .unwrap();
        let err = resolve_entry(dir.path(), "link.py").unwrap_err();
        assert!(
            err.contains("escapes") || err.contains("python compute path"),
            "{err}"
        );
    }

    #[test]
    fn transform_adds_field_and_keeps_arity() {
        let dir = write_py_dir(&[(
            "stamp.py",
            "def transform(record):\n    record[\"mapped\"] = True\n    return record\n",
        )]);
        let load = load_named(dir.path(), "stamp.py", "transform", "enrich");
        let out = apply_py_records(&[load], vec![json!({"id": "1"}), json!({"id": "2"})]).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].as_ref().unwrap()["mapped"], true);
        assert_eq!(out[1].as_ref().unwrap()["id"], "2");
    }

    #[test]
    fn sibling_import_sets_detail_site() {
        let dir = write_py_dir(&[
            (
                "lookup.py",
                "def lookup(device_id=None, asset_id=None, at=None):\n    if device_id == \"dev-1\":\n        return \"north\"\n    if asset_id == \"asset-9\":\n        return \"east\"\n    return None\n",
            ),
            (
                "enrich.py",
                "from lookup import lookup\n\ndef transform(record):\n    detail = record.setdefault(\"detail\", {})\n    if detail.get(\"site\"):\n        return record\n    site = lookup(device_id=record.get(\"device_id\"), asset_id=(record.get(\"truck\") or {}).get(\"id\"), at=record.get(\"event_time\"))\n    if site:\n        detail[\"site\"] = site\n    return record\n",
            ),
        ]);
        let load = load_named(dir.path(), "enrich.py", "transform", "enrich");
        let out = apply_py_records(
            &[load],
            vec![
                json!({"device_id": "dev-1"}),
                json!({"truck": {"id": "asset-9"}}),
                json!({"id": "none"}),
                json!({"device_id": "dev-1", "detail": {"site": "kept"}}),
            ],
        )
        .unwrap();
        assert_eq!(out[0].as_ref().unwrap()["detail"]["site"], "north");
        assert_eq!(out[1].as_ref().unwrap()["detail"]["site"], "east");
        assert!(out[2]
            .as_ref()
            .unwrap()
            .get("detail")
            .and_then(|d| d.get("site"))
            .is_none());
        assert_eq!(out[3].as_ref().unwrap()["detail"]["site"], "kept");
    }

    #[test]
    fn transform_error_is_per_record() {
        let dir = write_py_dir(&[(
            "maybe.py",
            "def transform(record):\n    if record.get(\"bad\"):\n        raise ValueError(\"nope\")\n    return record\n",
        )]);
        let load = load_named(dir.path(), "maybe.py", "transform", "enrich");
        let out =
            apply_py_records(&[load], vec![json!({"ok": true}), json!({"bad": true})]).unwrap();
        assert!(out[0].is_ok());
        assert!(out[1].as_ref().unwrap_err().contains("ValueError"));
    }

    #[test]
    fn missing_handler_fails_the_batch() {
        let dir = write_py_dir(&[("empty.py", "x = 1\n")]);
        let load = load_named(dir.path(), "empty.py", "transform", "enrich");
        let err = apply_py_records(&[load], vec![json!({"a": 1})]).unwrap_err();
        assert!(err.contains("not callable"), "{err}");
    }

    #[test]
    fn sibling_change_and_handler_rename_change_digest() {
        let dir = write_py_dir(&[
            ("lookup.py", "SITE = \"north\"\n"),
            ("enrich.py", "def transform(record):\n    return record\n"),
        ]);
        let first = admit_item(dir.path(), "enrich.py", "transform").unwrap();
        std::fs::write(dir.path().join("lookup.py"), "SITE = \"south\"\n").unwrap();
        let after_sibling = admit_item(dir.path(), "enrich.py", "transform").unwrap();
        assert_ne!(first.digest, after_sibling.digest);
        std::fs::write(
            dir.path().join("enrich.py"),
            "def enrich_event(record):\n    return record\n",
        )
        .unwrap();
        let after_handler = admit_item(dir.path(), "enrich.py", "enrich_event").unwrap();
        assert_ne!(after_sibling.digest, after_handler.digest);
        let fp1 = copy_wal_fingerprint(&[load_named(
            dir.path(),
            "enrich.py",
            "enrich_event",
            "enrich",
        )]);
        assert!(fp1.starts_with("copy#"));
        assert_ne!(fp1, COPY_WAL_FINGERPRINT);
        assert_eq!(copy_wal_fingerprint(&[]), COPY_WAL_FINGERPRINT);
    }
}
