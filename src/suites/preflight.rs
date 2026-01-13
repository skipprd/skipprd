use async_trait::async_trait;
use datafusion::prelude::SessionContext;

#[derive(Clone)]
pub struct PreflightBundle {
    pub discovery: crate::flows::discovery::DiscoveryBundle,
    pub preflight: Option<crate::flows::preflight::PreflightOutcome>,
}

#[async_trait]
pub trait PreflightProvider: Send + Sync {
    async fn run(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        ctx_df: &SessionContext,
    ) -> PreflightBundle;
}

/// Default Skippr preflight provider, backed by the existing catalog + discovery codepaths.
#[derive(Default)]
pub struct CatalogPreflightProvider {
    pub discovery_limits: crate::flows::discovery::DiscoveryLimits,
    pub run_preflight_on_bundle: bool,
}

#[async_trait]
impl PreflightProvider for CatalogPreflightProvider {
    async fn run(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        ctx_df: &SessionContext,
    ) -> PreflightBundle {
        let discovery = crate::flows::discovery::run_discovery(thread_id, question, ctx_df, &self.discovery_limits).await;
        let preflight = if self.run_preflight_on_bundle {
            Some(crate::flows::preflight::run_preflight_on_bundle(thread_id, agent_type).await)
        } else {
            None
        };
        PreflightBundle { discovery, preflight }
    }
}

