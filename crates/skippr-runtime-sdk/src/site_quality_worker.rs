//! Resolve the Site Quality Node worker script path for local dev and packaged runs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const WORKER_SCRIPT_FILE: &str = "site-quality-worker.mjs";
pub const REPO_WORKER_REL_PATH: &str =
    "plugins/data_source/site_quality/worker/site-quality-worker.mjs";

/// Optional override via `SKIPPR_SITE_QUALITY_WORKER_SCRIPT`, then search:
/// - plugin crate `worker/` (compile-time root when provided)
/// - skipprd repo layout under ancestors of the plugin executable and cwd
/// - repo root inferred from `SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR`
pub fn resolve_site_quality_worker_script(
    plugin_crate_root: Option<&Path>,
) -> Result<PathBuf, std::io::Error> {
    if let Ok(path) = std::env::var("SKIPPR_SITE_QUALITY_WORKER_SCRIPT") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            let path = PathBuf::from(trimmed);
            if path.is_file() {
                return Ok(path);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "SKIPPR_SITE_QUALITY_WORKER_SCRIPT points to missing file: {}",
                    path.display()
                ),
            ));
        }
    }

    for candidate in worker_script_candidates(plugin_crate_root) {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!(
            "site quality worker script not found (expected {REPO_WORKER_REL_PATH} in a skipprd checkout, or set SKIPPR_SITE_QUALITY_WORKER_SCRIPT)"
        ),
    ))
}

fn worker_script_candidates(plugin_crate_root: Option<&Path>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    let mut push = |path: PathBuf| {
        if seen.insert(path.clone()) {
            out.push(path);
        }
    };

    if let Some(root) = plugin_crate_root {
        push(root.join("worker").join(WORKER_SCRIPT_FILE));
    }

    if let Ok(manifest_dir) = std::env::var("SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR") {
        let manifests = PathBuf::from(manifest_dir);
        // <repo>/.skippr/local-runtime-plugins/manifests
        if let Some(repo_root) = manifests
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            push(repo_root.join(REPO_WORKER_REL_PATH));
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        for base in exe.ancestors() {
            push_repo_worker_candidates(base, &mut push);
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        for base in cwd.ancestors() {
            push_repo_worker_candidates(base, &mut push);
        }
    }

    out
}

fn push_repo_worker_candidates(base: &Path, push: &mut dyn FnMut(PathBuf)) {
    push(base.join(REPO_WORKER_REL_PATH));
    if base.file_name().is_some_and(|name| name == "site_quality") {
        push(base.join("worker").join(WORKER_SCRIPT_FILE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn resolves_from_plugin_crate_root() {
        let _guard = env_lock();
        std::env::remove_var("SKIPPR_SITE_QUALITY_WORKER_SCRIPT");
        let temp = tempfile::tempdir().unwrap();
        let worker_dir = temp.path().join("worker");
        fs::create_dir_all(&worker_dir).unwrap();
        let script = worker_dir.join(WORKER_SCRIPT_FILE);
        fs::write(&script, "// test").unwrap();
        let resolved =
            resolve_site_quality_worker_script(Some(temp.path())).expect("resolve worker");
        assert_eq!(resolved, script);
    }

    #[test]
    fn env_override_wins() {
        let _guard = env_lock();
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("custom.mjs");
        fs::write(&script, "// custom").unwrap();
        std::env::set_var(
            "SKIPPR_SITE_QUALITY_WORKER_SCRIPT",
            script.to_str().unwrap(),
        );
        let resolved = resolve_site_quality_worker_script(None).expect("resolve worker");
        assert_eq!(resolved, script);
        std::env::remove_var("SKIPPR_SITE_QUALITY_WORKER_SCRIPT");
    }
}
