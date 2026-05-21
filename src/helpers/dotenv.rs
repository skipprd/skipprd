use std::path::{Path, PathBuf};

/// Load `.env` then `.env.local` beside the Skippr manifest before resolving `${VAR}` in YAML.
///
/// - `.env` fills variables that are unset or empty in the process environment.
/// - `.env.local` overrides any variable named in that file (typical gitignored local secrets).
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn dotenv_fills_unset_and_empty_variables() {
        const VAR: &str = "SKIPPR_DOTENV_EMPTY_OVERRIDE_TEST";
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yml");
        std::fs::write(dir.path().join(".env"), format!("{VAR}=from-dot-env\n")).unwrap();
        std::fs::write(&config, "skippr:\n  workspace: demo\npipelines: {}\n").unwrap();

        std::env::set_var(VAR, "");
        load_dotenv_for_config_yaml_path(&config);
        assert_eq!(
            std::env::var(VAR).expect("var"),
            "from-dot-env",
            "empty process env should be replaced from .env"
        );
        std::env::remove_var(VAR);
    }

    #[test]
    fn dotenv_local_overrides_dot_env() {
        const VAR: &str = "SKIPPR_DOTENV_LOCAL_OVERRIDE_TEST";
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yml");
        std::fs::write(dir.path().join(".env"), format!("{VAR}=from-dot-env\n")).unwrap();
        std::fs::write(dir.path().join(".env.local"), format!("{VAR}=from-local\n")).unwrap();
        std::fs::write(&config, "skippr:\n  workspace: demo\npipelines: {}\n").unwrap();

        std::env::remove_var(VAR);
        load_dotenv_for_config_yaml_path(&config);
        assert_eq!(std::env::var(VAR).expect("var"), "from-local");
        std::env::remove_var(VAR);
    }

    #[test]
    fn dotenv_does_not_override_nonempty_process_env() {
        const VAR: &str = "SKIPPR_DOTENV_KEEP_PROCESS_TEST";
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yml");
        std::fs::write(dir.path().join(".env"), format!("{VAR}=from-dot-env\n")).unwrap();
        std::fs::write(&config, "skippr:\n  workspace: demo\npipelines: {}\n").unwrap();

        std::env::set_var(VAR, "from-shell");
        load_dotenv_for_config_yaml_path(&config);
        assert_eq!(std::env::var(VAR).expect("var"), "from-shell");
        std::env::remove_var(VAR);
    }

    #[test]
    fn config_parent_dir_defaults_to_dot_for_bare_filename() {
        let dir = config_parent_dir(Path::new("skippr.yml"));
        assert_eq!(dir, PathBuf::from("."));
        let _ = OsString::from("skippr.yml");
    }
}
