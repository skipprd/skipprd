use std::collections::HashMap;
use tracing::{info, warn};

pub struct Orchestrator;

impl Orchestrator {
    pub async fn build_all_with_progress(
        namespaces: &HashMap<String, crate::discover::Metadata>,
        progress: Option<&crate::helpers::progress::ProgressUi>
    ) {
        // Prefer namespaces from registry to ensure S3-backed list, fallback to metadata keys
        let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
        info!("ORCHESTRATOR: building catalogs for all namespaces in pipeline '{}'", pipeline);
        let mut regs = crate::sql::registry::list_namespaces(&pipeline).await;
        if regs.is_empty() {
            regs = namespaces.keys().cloned().collect();
        }
        regs.sort();
        info!("ORCHESTRATOR: building catalogs for {} namespaces", regs.len());
        let ctx = crate::catalog::session::SessionFactory::new_context().await;
        let mut to_write: Vec<(String, crate::catalog::model::DataCatalog)> = Vec::new();

        if let Some(p) = progress { p.start("Building stats"); }
        for ns in regs.iter() {
            info!("ORCHESTRATOR: ns='{}' registering view", &ns);
            if let Err(e) = crate::sql::tables::register_namespace_view(&ctx, &pipeline, &ns).await {
                warn!("ORCHESTRATOR: ns='{}' register view failed: {}", ns, e);
                continue;
            }
            info!("ORCHESTRATOR: ns='{}' computing stats", ns);
            match crate::catalog::stats_builder::StatsBuilder::compute(&ctx, &ns, 10).await {
                Ok((ns_stats, ds_stats)) => {
                    let cat = crate::catalog::catalog::CatalogBuilder::build_with_stats(&ns, Some(ns_stats), Some(ds_stats)).await;
                    to_write.push((ns.clone(), cat));
                    info!("ORCHESTRATOR: ns='{}' stats built", ns);
                }
                Err(e) => {
                    warn!("ORCHESTRATOR: ns='{}' stats failed: {}", ns, e);
                }
            }
        }
        if let Some(p) = progress { p.complete("Building stats"); }

        if let Some(p) = progress { p.start("Building catalog"); }
        for (ns, cat) in to_write.into_iter() {
            // Print final catalog JSON once per ns before write
            match serde_json::to_string_pretty(&serde_json::to_value(&cat).unwrap_or(serde_json::Value::Null)) {
                Ok(pretty) => info!("FINAL Catalog ns='{}':\n{}", ns, pretty),
                Err(_) => info!("FINAL Catalog ns='{}': <failed to stringify>", ns),
            }
            crate::helpers::configuration::Config::write_catalog_async(&ns, &cat).await;
        }
        if let Some(p) = progress { p.complete("Building catalog"); }
        info!("ORCHESTRATOR: completed building catalogs for all namespaces");
    }
    pub async fn build_all(namespaces: &HashMap<String, crate::discover::Metadata>) {
        Self::build_all_with_progress(namespaces, None).await;
    }
}


