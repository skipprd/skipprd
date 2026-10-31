//! Open Iceberg catalogs from serialized [`IcebergCatalogSpec`] identity.

use std::sync::Arc;

use super::iceberg_lake::IcebergCatalogSpec;

async fn open_skippr_catalog_backend(
    backend: skippr_iceberg_catalog::SkipprCatalogBackend,
    cfg: &skippr_iceberg_catalog::SkipprLakeConfig,
) -> Result<Arc<dyn iceberg::Catalog>, String> {
    match backend {
        skippr_iceberg_catalog::SkipprCatalogBackend::DynamoDb => {
            #[cfg(feature = "offset-store-dynamodb")]
            {
                let catalog = skippr_iceberg_catalog_dynamodb::DynamoDbCatalog::new(cfg)
                    .await
                    .map_err(|err| err.to_string())?;
                Ok(Arc::new(catalog))
            }
            #[cfg(not(feature = "offset-store-dynamodb"))]
            {
                let _ = cfg;
                Err("SkipprLake catalog requires --features offset-store-dynamodb".into())
            }
        }
    }
}

pub async fn open_iceberg_catalog(
    spec: &IcebergCatalogSpec,
) -> Result<Arc<dyn iceberg::Catalog>, String> {
    open_iceberg_catalog_spec(spec).await
}

async fn open_iceberg_catalog_spec(
    spec: &IcebergCatalogSpec,
) -> Result<Arc<dyn iceberg::Catalog>, String> {
    match spec {
        IcebergCatalogSpec::Skippr(open) => {
            open_skippr_catalog_backend(open.backend, &open.lake).await
        }
        IcebergCatalogSpec::Glue(cfg) => {
            skippr_iceberg_catalog_glue::open(&skippr_iceberg_catalog_glue::GlueOpen {
                warehouse: cfg.warehouse.clone(),
                region: cfg.region.clone(),
                catalog_id: cfg.catalog_id.clone(),
                object_store: cfg.object_store.clone(),
            })
            .await
        }
        IcebergCatalogSpec::Filesystem(cfg) => {
            skippr_iceberg_catalog::validate_file_warehouse(&cfg.warehouse, &cfg.table_namespace)?;
            let file_io = iceberg::io::FileIO::new_with_fs();
            let catalog = skippr_iceberg_catalog_fs::FsCatalog::new(cfg.warehouse.clone(), file_io)
                .map_err(|err| err.to_string())?;
            Ok(Arc::new(catalog))
        }
    }
}

pub async fn reopen_iceberg_catalog(
    catalog_json: &str,
) -> Result<Arc<dyn iceberg::Catalog>, String> {
    let spec: IcebergCatalogSpec =
        serde_json::from_str(catalog_json).map_err(|err| err.to_string())?;
    open_iceberg_catalog_spec(&spec).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn skippr_catalog_open_is_dynamodb_from_serialized_identity() {
        let catalog = include_str!("iceberg_catalog.rs");
        assert!(
            !catalog.contains(concat!("pub async fn ", "open_skippr_catalog")),
            "Skippr catalog identity is SkipprLakeOpen.backend; Config must not select the backend at query/serve open"
        );
        assert!(
            catalog.contains("open_iceberg_catalog_spec"),
            "query/serve catalog open must not depend on process Config for Iceberg identity"
        );
        assert!(
            !catalog.contains(concat!("open_iceberg_catalog(&", "Config::new()")),
            "QueryRuntime reload must reopen from IcebergCatalogSpec, not Config::new()"
        );
        let host = include_str!("../runtime_plugins/host.rs");
        assert!(
            host.contains("SKIPPR_STORE_TYPE"),
            "runtime plugin spawn must inject SKIPPR_STORE_TYPE"
        );
        assert!(
            host.contains("skippr_store_type_value"),
            "runtime plugin spawn must inject the host-resolved SkipprStore type"
        );
        let plugin = include_str!("../../plugins/data_sink/skipprlake/src/lib.rs");
        assert!(
            plugin.contains("DynamoDbCatalog::new"),
            "plugin catalog must open DynamoDB"
        );
        assert!(
            !plugin.contains("from_store_type"),
            "plugin catalog must not branch on SkipprStore type"
        );
    }
}
