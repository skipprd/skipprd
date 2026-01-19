use async_trait::async_trait;

/// Minimal secrets provider.
///
/// This intentionally does *not* expose a global config surface. It is meant for
/// credentials and other sensitive values (API keys, OAuth tokens, service account
/// JSON, etc.).
#[async_trait]
pub trait SecretsProvider: Send + Sync {
    async fn get_secret(&self, name: &str) -> Result<Option<String>, String>;
}

/// Secrets provider that never returns any secret.
#[derive(Clone, Default)]
pub struct NullSecretsProvider;

#[async_trait]
impl SecretsProvider for NullSecretsProvider {
    async fn get_secret(&self, _name: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
}

