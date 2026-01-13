use async_trait::async_trait;

/// Configuration adapter interface.
///
/// Initial implementation is env-backed to preserve the current behavior/pattern.
#[async_trait]
pub trait ConfigAdapter: Send + Sync {
    fn getenv(&self, key: &str) -> Option<String>;
    fn getenv_or(&self, key: &str, default: &str) -> String;
}

#[derive(Clone, Default)]
pub struct EnvConfigAdapter;

#[async_trait]
impl ConfigAdapter for EnvConfigAdapter {
    fn getenv(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    fn getenv_or(&self, key: &str, default: &str) -> String {
        std::env::var(key).unwrap_or_else(|_| default.to_string())
    }
}

