use std::path::{Path, PathBuf};

/// Load `.env` then `.env.local` beside the Skippr manifest before resolving `${VAR}` in YAML.
pub fn load_dotenv_for_config_yaml_path(config_yaml_path: &Path) {
    let dir = config_parent_dir(config_yaml_path);
    load_dotenv_file(&dir.join(".env"), false);
    load_dotenv_file(&dir.join(".env.local"), true);
}

fn config_parent_dir(config_yaml_path: &Path) -> PathBuf {
    config_yaml_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn load_dotenv_file(path: &Path, override_existing: bool) {
    if !path.is_file() {
        return;
    }
    if override_existing {
        if let Err(err) = dotenvy::from_path_override(path) {
            eprintln!("skippr: warning: failed to load {}: {err}", path.display());
        }
        return;
    }
    match dotenvy::from_path_iter(path) {
        Ok(iter) => {
            for item in iter.flatten() {
                let (key, value) = item;
                match std::env::var(&key) {
                    Err(_) => std::env::set_var(&key, &value),
                    Ok(existing) if existing.is_empty() => std::env::set_var(&key, &value),
                    Ok(_) => {}
                }
            }
        }
        Err(err) => {
            eprintln!("skippr: warning: failed to load {}: {err}", path.display());
        }
    }
}
