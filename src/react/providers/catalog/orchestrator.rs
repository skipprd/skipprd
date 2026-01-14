use std::collections::HashMap;

use tracing::{info, warn};

pub struct Orchestrator;

impl Orchestrator {
    pub async fn build_all_with_progress(
        query: &dyn crate::react::providers::DatasetCatalogProvider,
        _namespaces: &HashMap<String, crate::discover::Metadata>,
        progress: Option<&crate::helpers::progress::ProgressUi>,
    ) -> Result<Vec<(String, super::types::DataCatalog)>, String> {
        info!("ORCHESTRATOR: building catalogs from provider dataset discovery");
        let mut datasets = query.list_datasets().await?;
        datasets.sort_by(|a, b| a.fqn().cmp(&b.fqn()));
        info!("ORCHESTRATOR: discovered {} dataset(s)", datasets.len());

        let mut to_write: Vec<(String, super::types::DataCatalog)> = Vec::new();

        if let Some(p) = progress {
            p.start("Building stats");
        }
        for ds in datasets.iter() {
            let ds_id = ds.fqn();
            info!("ORCHESTRATOR: dataset='{}' computing stats", ds_id);

            // Fetch schema once so we can embed types into the catalog fields.
            let schema_cols = match query.get_dataset_schema(ds).await {
                Ok(cols) => cols,
                Err(e) => {
                    warn!("ORCHESTRATOR: dataset='{}' schema unavailable: {}", ds_id, e);
                    Vec::new()
                }
            };
            let mut type_by_field: std::collections::HashMap<String, String> = std::collections::HashMap::new();
            for (name, ty) in schema_cols.iter() {
                type_by_field.insert(name.clone(), ty.clone());
            }

            // Prefer provider stats. If unavailable, fall back to schema-only catalog.
            let (ns_stats_opt, ds_stats_opt) = match query.get_dataset_stats(ds, 50).await {
                Ok((ns_stats, ds_stats)) => (Some(ns_stats), Some(ds_stats)),
                Err(e) => {
                    warn!("ORCHESTRATOR: dataset='{}' stats unavailable: {}", ds_id, e);
                    (None, None)
                }
            };

            // Ensure we can still build a usable catalog even without stats: seed fields from schema.
            let ns_stats_seeded: Option<crate::discover::stats::NamespaceStats> = if ns_stats_opt.is_some() {
                ns_stats_opt
            } else {
                if schema_cols.is_empty() {
                    None
                } else {
                    let mut ns = crate::discover::stats::NamespaceStats::new(&ds_id);
                    for (name, _ty) in schema_cols.iter() {
                        ns.fields
                            .entry(name.clone())
                            .or_insert_with(crate::discover::stats::FieldStats::default);
                    }
                    Some(ns)
                }
            };

            let mut cat = super::builder::CatalogBuilder::build_with_stats(ds, ns_stats_seeded, ds_stats_opt).await;
            // Fill in per-field type strings (best-effort).
            for f in cat.fields.iter_mut() {
                if f.data_type.is_none() {
                    if let Some(ty) = type_by_field.get(&f.name) {
                        f.data_type = Some(ty.clone());
                    }
                }
            }
            to_write.push((ds_id.clone(), cat));
            info!("ORCHESTRATOR: dataset='{}' catalog built", ds_id);
        }
        if let Some(p) = progress {
            p.complete("Building stats");
        }

        if let Some(p) = progress {
            p.start("Building catalog");
        }
        for (ds_id, cat) in to_write.iter() {
            // Print final catalog JSON once per ns before write
            match serde_json::to_string_pretty(&serde_json::to_value(&cat).unwrap_or(serde_json::Value::Null)) {
                Ok(pretty) => info!("FINAL Catalog dataset_id='{}':\n{}", ds_id, pretty),
                Err(_) => info!("FINAL Catalog dataset_id='{}': <failed to stringify>", ds_id),
            }
        }
        if let Some(p) = progress {
            p.complete("Building catalog");
        }
        info!("ORCHESTRATOR: completed building catalogs for all datasets");
        Ok(to_write)
    }

    // legacy wrapper removed
}

