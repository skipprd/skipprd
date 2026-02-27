use async_trait::async_trait;
pub use react_core::providers::secrets::SecretsProvider;

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
