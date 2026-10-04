use std::collections::HashMap;
use std::sync::Arc;

use iceberg::Catalog;
use iceberg::CatalogBuilder;
use iceberg_catalog_glue::{
    GlueCatalogBuilder, AWS_REGION_NAME, GLUE_CATALOG_PROP_CATALOG_ID, GLUE_CATALOG_PROP_WAREHOUSE,
};
use skippr_iceberg_catalog::{s3_object_store_props, S3CompatibleObjectStore};

/// Fields required to open a Glue Iceberg catalog. Athena workgroup/results stay on the plugin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlueOpen {
    pub warehouse: String,
    pub region: Option<String>,
    pub catalog_id: Option<String>,
    pub object_store: S3CompatibleObjectStore,
}

pub async fn open(cfg: &GlueOpen) -> Result<Arc<dyn Catalog>, String> {
    let mut props = HashMap::new();
    props.insert(
        GLUE_CATALOG_PROP_WAREHOUSE.to_string(),
        cfg.warehouse.clone(),
    );
    if let Some(id) = &cfg.catalog_id {
        if !id.trim().is_empty() {
            props.insert(GLUE_CATALOG_PROP_CATALOG_ID.to_string(), id.clone());
        }
    }
    if let Some(region) = &cfg.region {
        if !region.trim().is_empty() {
            props.insert(AWS_REGION_NAME.to_string(), region.clone());
        }
    }
    for (k, v) in s3_object_store_props(&cfg.object_store)? {
        props.insert(k, v);
    }
    let catalog = GlueCatalogBuilder::default()
        .load("glue", props)
        .await
        .map_err(|err| err.to_string())?;
    Ok(Arc::new(catalog))
}
