use tracing::{debug, info};

use crate::types::{DataCatalog, SemanticField, SemanticFieldRole};
use crate::utils::{classify_field, to_stats_lite};

pub struct CatalogBuilder;

impl CatalogBuilder {
    pub async fn build_with_stats(
        dataset: &react_suite_data_engineer::providers::DatasetId,
        ns_stats: Option<react_suite_data_engineer::providers::DatasetFieldStats>,
        dataset_stats: Option<crate::types::DatasetStats>,
    ) -> DataCatalog {
        let (semantic_fields, semantic_dims, semantic_metrics): (
            Vec<SemanticField>,
            Vec<String>,
            Vec<String>,
        ) = {
            if let Some(ns) = ns_stats.as_ref() {
                let mut fields: Vec<SemanticField> = Vec::new();
                for (fname, fstats) in ns.fields.iter() {
                    let role = classify_field(fname, Some(fstats));
                    fields.push(SemanticField {
                        name: fname.clone(),
                        role,
                    });
                }
                let mut dims: Vec<String> = Vec::new();
                let mut mets: Vec<String> = Vec::new();
                for f in &fields {
                    match f.role {
                        SemanticFieldRole::Id
                        | SemanticFieldRole::Timestamp
                        | SemanticFieldRole::Categorical => dims.push(f.name.clone()),
                        SemanticFieldRole::Metric => mets.push(f.name.clone()),
                        SemanticFieldRole::FreeText => {}
                    }
                }
                (fields, dims, mets)
            } else {
                (Vec::new(), Vec::new(), Vec::new())
            }
        };
        fn build_structure_index(
            names: &[String],
        ) -> std::collections::HashMap<String, Vec<String>> {
            let mut idx: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            for n in names {
                let parts: Vec<&str> = n.split('.').collect();
                for i in 0..parts.len().saturating_sub(1) {
                    let parent = parts[0..=i].join(".");
                    let child = parts[0..=i + 1].join(".");
                    let e = idx.entry(parent).or_insert_with(Vec::new);
                    if !e.iter().any(|x| x == &child) {
                        e.push(child);
                    }
                }
            }
            for (_k, v) in idx.iter_mut() {
                v.sort();
                v.dedup();
            }
            idx
        }
        let mut catalog = DataCatalog {
            dataset_id: dataset.fqn(),
            catalog: dataset.catalog.clone(),
            database: dataset.database.clone(),
            table: dataset.table.clone(),
            description: None,
            dimensions: semantic_dims.clone(),
            metrics: semantic_metrics.clone(),
            fields: semantic_fields
                .iter()
                .map(|f| crate::types::CatalogField {
                    entity: String::new(),
                    name: f.name.clone(),
                    data_type: None,
                    root_column: None,
                    field_path: None,
                    structure_kind: None,
                    access_descriptor: None,
                    description: None,
                    synonyms: None,
                    pii_sensitivity: None,
                    units_or_format: None,
                    role: Some(format!("{:?}", f.role)),
                    stats: None,
                })
                .collect(),
            structure_index: std::collections::HashMap::new(),
            dataset_stats,
            built_at_epoch_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs()),
        };
        if let Some(ns) = ns_stats.as_ref() {
            let mut nulls_by_field: std::collections::HashMap<String, u64> =
                std::collections::HashMap::new();
            for cf in catalog.fields.iter_mut() {
                if let Some(fs) = ns.fields.get(&cf.name) {
                    nulls_by_field.insert(cf.name.clone(), fs.nulls);
                    cf.stats = Some(to_stats_lite(fs));
                }
            }
            if let Some(ds) = catalog.dataset_stats.as_mut() {
                ds.nulls_by_field = nulls_by_field;
            }
        }
        let field_names: Vec<String> = catalog.fields.iter().map(|f| f.name.clone()).collect();
        catalog.structure_index = build_structure_index(&field_names);
        let ds_id = dataset.fqn();
        info!(
            "Catalog built dataset_id='{}' fields={}",
            ds_id,
            catalog.fields.len()
        );
        debug!(
            "Catalog field sample dataset_id='{}' sample=[{}]",
            ds_id,
            catalog
                .fields
                .iter()
                .take(8)
                .map(|f| f.name.clone())
                .collect::<Vec<_>>()
                .join(",")
        );
        catalog
    }
}
