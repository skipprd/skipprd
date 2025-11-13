use std::collections::HashMap;
use tracing::{info, warn};

pub struct Orchestrator;

impl Orchestrator {
    pub async fn build(namespace: &str) {
        let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
        info!("ORCHESTRATOR: ns='{}' begin", namespace);
        // Use a single session context for registration and stats to ensure the view is visible
        let ctx = crate::catalog::session::SessionFactory::new_context().await;
        info!("ORCHESTRATOR: ns='{}' registering view", namespace);
        if let Err(e) = crate::sql::tables::register_namespace_view(&ctx, &pipeline, namespace).await {
            warn!("ORCHESTRATOR: ns='{}' register view failed: {}", namespace, e);
            return;
        }
        info!("ORCHESTRATOR: ns='{}' computing stats", namespace);
        match crate::catalog::stats_builder::StatsBuilder::compute(&ctx, namespace, 10).await {
            Ok((ns_stats, ds_stats)) => {
                info!("ORCHESTRATOR: ns='{}' writing catalog", namespace);
                crate::catalog::catalog::CatalogBuilder::build_and_write_with_stats(namespace, Some(ns_stats), Some(ds_stats)).await;
                info!("ORCHESTRATOR: ns='{}' catalog built", namespace);
            }
            Err(e) => {
                warn!("ORCHESTRATOR: ns='{}' stats failed: {}", namespace, e);
            }
        }
    }

    pub async fn build_all(namespaces: &HashMap<String, crate::discover::Metadata>) {
        // Prefer namespaces from registry to ensure S3-backed list, fallback to metadata keys
        let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
        info!("ORCHESTRATOR: building catalogs for all namespaces in pipeline '{}'", pipeline);
        let mut regs = crate::sql::registry::list_namespaces(&pipeline).await;
        if regs.is_empty() {
            regs = namespaces.keys().cloned().collect();
        }
        regs.sort();
        info!("ORCHESTRATOR: building catalogs for {} namespaces", regs.len());
        for ns in regs {
            info!("ORCHESTRATOR: ns='{}' registering view", &ns);
            // Each ns bounded with timeout so one bad ns doesn't stall all
            let build_res = tokio::time::timeout(std::time::Duration::from_secs(120), Self::build(&ns)).await;
            match build_res {
                Ok(_) => {}
                Err(_) => warn!("ORCHESTRATOR: ns='{}' timed out", &ns),
            }
        }
        info!("ORCHESTRATOR: completed building catalogs for all namespaces");
    }
}


