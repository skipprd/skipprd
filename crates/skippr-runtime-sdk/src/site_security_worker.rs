//! Resolve the Site Security Node worker script path for local dev and packaged runs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const WORKER_SCRIPT_FILE: &str = "site-security-worker.mjs";
pub const REPO_WORKER_REL_PATH: &str =
    "plugins/data_source/site_security/worker/site-security-worker.mjs";

pub fn resolve_site_security_worker_script(
    plugin_crate_root: Option<&Path>,
) -> Result<PathBuf, std::io::Error> {
    if let Ok(path) = std::env::var("SKIPPR_SITE_SECURITY_WORKER_SCRIPT") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            let path = PathBuf::from(trimmed);
            if path.is_file() {
                return Ok(path);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "SKIPPR_SITE_SECURITY_WORKER_SCRIPT points to missing file: {}",
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
            "site security worker script not found (expected {REPO_WORKER_REL_PATH} in a skipprd checkout, or set SKIPPR_SITE_SECURITY_WORKER_SCRIPT)"
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
    if base.file_name().is_some_and(|name| name == "site_security") {
        push(base.join("worker").join(WORKER_SCRIPT_FILE));
    }
}
