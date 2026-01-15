use async_trait::async_trait;

#[derive(Clone)]
pub struct PreflightBundle {
    pub discovery: crate::preflight::discovery::DiscoveryBundle,
    pub preflight: Option<crate::preflight::catalog_preflight::PreflightOutcome>,
}

#[async_trait]
pub trait PreflightProvider: Send + Sync {
    async fn run(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        sctx: &crate::suites::SuiteCtx,
    ) -> PreflightBundle;
}

/// Default preflight provider, backed by the existing catalog + discovery codepaths.
#[derive(Default)]
pub struct CatalogPreflightProvider {
    pub discovery_limits: crate::preflight::discovery::DiscoveryLimits,
    pub run_preflight_on_bundle: bool,
}

#[async_trait]
impl PreflightProvider for CatalogPreflightProvider {
    async fn run(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        sctx: &crate::suites::SuiteCtx,
    ) -> PreflightBundle {
        let discovery = crate::preflight::discovery::run_discovery(question, &self.discovery_limits, sctx).await;
        let preflight = if self.run_preflight_on_bundle {
            Some(crate::preflight::catalog_preflight::run_preflight_on_bundle(thread_id, agent_type).await)
        } else {
            None
        };
        PreflightBundle { discovery, preflight }
    }
}

