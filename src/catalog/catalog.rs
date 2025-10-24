pub struct CatalogBuilder;

impl CatalogBuilder {
    pub async fn build_and_write(namespace: &str) {
        let semantic = crate::semantics::infer::infer_semantic_model(namespace);
        let catalog = crate::semantics::model::DataCatalog {
            namespace: namespace.to_string(),
            description: None,
            fields: semantic.fields.iter().map(|f| crate::semantics::model::CatalogField {
                entity: String::new(),
                name: f.name.clone(),
                description: None,
                synonyms: None,
                pii_sensitivity: None,
                units_or_format: None,
            }).collect(),
        };
        crate::helpers::configuration::Config::write_catalog_async(namespace, &catalog).await;
    }
}
