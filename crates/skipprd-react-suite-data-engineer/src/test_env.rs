//! Serializes process environment mutations across parallel unit tests.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

pub fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap()
}

pub fn with_env(vars: &[(&str, Option<&str>)], test: impl FnOnce()) {
    let _guard = lock();
    let saved: Vec<(String, Option<String>)> = vars
        .iter()
        .map(|(key, _)| ((*key).to_string(), std::env::var(key).ok()))
        .collect();

    for (key, value) in vars {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    test();

    for (key, value) in saved {
        match value {
            Some(value) => std::env::set_var(&key, value),
            None => std::env::remove_var(&key),
        }
    }
}

pub fn with_local_dbt_root(root: &Path, test: impl FnOnce()) {
    let _guard = lock();
    let saved_local = std::env::var("SKIPPR_LOCAL_DBT_PROJECT_ROOT").ok();
    std::env::set_var("SKIPPR_LOCAL_DBT_PROJECT_ROOT", root);

    test();

    match saved_local {
        Some(value) => std::env::set_var("SKIPPR_LOCAL_DBT_PROJECT_ROOT", value),
        None => std::env::remove_var("SKIPPR_LOCAL_DBT_PROJECT_ROOT"),
    }
}

pub fn with_no_local_dbt_root(test: impl FnOnce()) {
    with_env(&[("SKIPPR_LOCAL_DBT_PROJECT_ROOT", None)], test);
}
