use tracing::debug;

pub struct CatalogBuilder;

impl CatalogBuilder {
    pub async fn build_and_write(namespace: &str) {
        let semantic = crate::catalog::infer::infer_semantic_model_async(namespace).await;
        let mut catalog = crate::catalog::model::DataCatalog {
            namespace: namespace.to_string(),
            description: None,
            dimensions: semantic.dimensions.clone(),
            metrics: semantic.metrics.clone(),
            fields: semantic.fields.iter().map(|f| crate::catalog::model::CatalogField {
                entity: String::new(),
                name: f.name.clone(),
                description: None,
                synonyms: None,
                pii_sensitivity: None,
                units_or_format: None,
                role: Some(format!("{:?}", f.role)),
            }).collect(),
        };
        debug!("META: build catalog ns='{}' fields={} sample=[{}]", namespace, catalog.fields.len(), catalog.fields.iter().take(8).map(|f| f.name.clone()).collect::<Vec<_>>().join(","));

        // Defer field-level LLM enrichment to end-of-discover pass

        crate::helpers::configuration::Config::write_catalog_async(namespace, &catalog).await;
    }
}
