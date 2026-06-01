pub use skippr_runtime_sdk::RUNNING;
pub use skippr_runtime_sdk::{converters, discover, helpers, ingest, metrics, plugins, serdes};

pub mod bing;
mod bing_api;
pub mod streams;

#[cfg(test)]
pub(crate) mod test_env {
    use std::sync::{LazyLock, Mutex};

    static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    pub fn lock() -> std::sync::MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_fixture_dir() {
        std::env::set_var(
            "SKIPPR_BING_WEBMASTER_TOOLS_FIXTURE_DIR",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures"),
        );
    }

    pub fn clear_fixture_dir() {
        std::env::remove_var("SKIPPR_BING_WEBMASTER_TOOLS_FIXTURE_DIR");
    }

    pub fn clear_discover_mode() {
        std::env::remove_var(skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV);
    }
}

pub use bing::*;
