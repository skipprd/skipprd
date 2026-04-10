#![allow(dead_code)]

use std::path::PathBuf;

pub fn manifest_path_from_env(env_var: &str, default_relative_path: &str) -> PathBuf {
    std::env::var_os(env_var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(default_relative_path))
}
