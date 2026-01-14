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

/// Env-backed secrets provider.
///
/// Useful for local dev and for preserving current behavior while wiring proper
/// secret managers later (GCP Secret Manager, AWS Secrets Manager, Vault, etc.).
#[derive(Clone, Default)]
pub struct EnvSecretsProvider;

#[async_trait]
impl SecretsProvider for EnvSecretsProvider {
    async fn get_secret(&self, name: &str) -> Result<Option<String>, String> {
        Ok(std::env::var(name).ok())
    }
}

