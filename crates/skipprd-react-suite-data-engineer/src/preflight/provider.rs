use async_trait::async_trait;

#[derive(Clone)]
pub struct PreflightBundle {
    pub discovery: super::discovery::DiscoveryBundle,
}

#[async_trait]
pub trait PreflightProvider: Send + Sync {
    async fn run(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        sctx: &react_core::suite::SuiteCtx,
    ) -> PreflightBundle;
}

/// Default preflight provider, backed by the existing catalog + discovery codepaths.
#[derive(Default)]
pub struct CatalogPreflightProvider {
    pub discovery_limits: super::discovery::DiscoveryLimits,
}

#[async_trait]
impl PreflightProvider for CatalogPreflightProvider {
    async fn run(
        &self,
        thread_id: &str,
        question: &str,
        _agent_type: &str,
        sctx: &react_core::suite::SuiteCtx,
    ) -> PreflightBundle {
        let discovery = super::discovery::run_discovery_cached(
            thread_id,
            question,
            &self.discovery_limits,
            sctx,
        )
        .await;
        PreflightBundle { discovery }
    }
}
