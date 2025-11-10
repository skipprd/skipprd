use std::collections::HashMap;
use tracing::debug;

pub struct Orchestrator;

impl Orchestrator {
    pub async fn build(namespace: &str) {
        let ctx = crate::catalog::session::SessionFactory::new_context().await;
        crate::catalog::session::SessionFactory::register_s3_and_wal(&ctx, namespace).await;

        // Register S3 tables using manifest-backed registry prefixes
        let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
        if let Some(entry) = crate::sql::registry::find_entry(&pipeline, namespace).await {
            let prefixes = entry.data_prefixes;
            for (idx, path) in prefixes.iter().enumerate() {
                let tname = format!("{}_s3_{}", namespace, idx);
                let _ = ctx.register_parquet(&tname, path, datafusion::prelude::ParquetReadOptions::default()).await;
            }
            debug!("META: orchestrator ns='{}' registered_sources={}", namespace, prefixes.len());
        } else {
            debug!("META: orchestrator ns='{}' no registry prefixes found", namespace);
        }

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


