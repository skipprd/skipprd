use std::collections::HashMap;
use tracing::debug;

pub struct Orchestrator;

impl Orchestrator {
    pub async fn build(namespace: &str) {
        let ctx = crate::catalog::session::SessionFactory::new_context().await;
        let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
        let _ = crate::sql::tables::register_namespace_view(&ctx, &pipeline, namespace).await;

        // Stats → Catalog (+LLM in build_catalog tail)
        // Full S3 scan for authoritative stats (0 => no limit). For now, apply a small limit for fast runs.
        crate::catalog::stats_builder::StatsBuilder::compute_and_write(&ctx, namespace, 10).await.expect("TODO: panic message");
        crate::catalog::catalog::CatalogBuilder::build_and_write(namespace).await;
        // Defer dataset-level LLM enrichment to the end-of-discover pass
    }

    pub async fn build_all(namespaces: &HashMap<String, crate::discover::Metadata>) {
        // Prefer namespaces from registry to ensure S3-backed list, fallback to metadata keys
        let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
        let mut regs = crate::sql::registry::list_namespaces(&pipeline).await;
        if regs.is_empty() {
            regs = namespaces.keys().cloned().collect();
        }
        for ns in regs { Self::build(&ns).await; }
    }
}


