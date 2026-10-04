//! Open clustered control-plane stores (DynamoDB or Cloud tables).

use std::sync::Arc;

use skippr_lease::{ClusterMembershipStore, PipelineLeaseStore};

use crate::cluster::identity::ClusterConfig;
use crate::helpers::configuration::Config;
use crate::helpers::wal_storage::SkipprStoreKind;

pub async fn open_lease_store(
    config: &Config,
    table: String,
) -> Result<Arc<dyn PipelineLeaseStore>, String> {
    let kind = crate::pipeline_backend::configured_kind(config)?;
    match kind {
        SkipprStoreKind::Sled => Err(
            "clustered pipeline leases require dynamodb or cloud-tables; sled is the Disk/S3 lease backend"
                .into(),
        ),
        SkipprStoreKind::CloudTables | SkipprStoreKind::DynamoDb => {
            crate::pipeline_backend::open_pipeline_lease_store(kind, None, table).await
        }
    }
}

pub async fn open_membership_store(
    config: &Config,
    table: String,
) -> Result<Arc<dyn ClusterMembershipStore>, String> {
    match crate::pipeline_backend::configured_kind(config)? {
        SkipprStoreKind::CloudTables => {
            #[cfg(feature = "offset-store-cloud-tables")]
            {
                let store =
                    skippr_lease_store_cloud_tables::CloudTablesMembershipStore::connect(table)
                        .await
                        .map_err(|err| err.to_string())?;
                return Ok(Arc::new(store));
            }
            #[cfg(not(feature = "offset-store-cloud-tables"))]
            {
                let _ = table;
                Err("skippr.store.type=cloud-tables requires --features offset-store-cloud-tables".into())
            }
        }
        SkipprStoreKind::DynamoDb => {
            #[cfg(feature = "offset-store-dynamodb")]
            {
                let store = skippr_lease_store_dynamodb::DynamoDbMembershipStore::connect(table)
                    .await
                    .map_err(|err| err.to_string())?;
                Ok(Arc::new(store))
            }
            #[cfg(not(feature = "offset-store-dynamodb"))]
            {
                let _ = table;
                Err("clustered membership requires --features offset-store-dynamodb".into())
            }
        }
        SkipprStoreKind::Sled => Err(
            "clustered membership requires dynamodb or cloud-tables; sled is the Disk/S3 lease backend"
                .into(),
        ),
    }
}

pub fn uses_cloud_tables(config: &Config) -> bool {
    catalog_backend(config) == skippr_iceberg_catalog::SkipprCatalogBackend::CloudTables
}

fn catalog_backend(config: &Config) -> skippr_iceberg_catalog::SkipprCatalogBackend {
    skippr_iceberg_catalog::SkipprCatalogBackend::from_store_type(
        &crate::pipeline_backend::skippr_store_type_value(config),
    )
}

async fn open_skippr_catalog_backend(
    backend: skippr_iceberg_catalog::SkipprCatalogBackend,
    cfg: &skippr_iceberg_catalog::SkipprLakeConfig,
) -> Result<Arc<dyn iceberg::Catalog>, String> {
    match backend {
        skippr_iceberg_catalog::SkipprCatalogBackend::CloudTables => {
            #[cfg(feature = "offset-store-cloud-tables")]
            {
                let catalog = skippr_iceberg_catalog_cloud_tables::CloudTablesCatalog::new(cfg)
                    .await
                    .map_err(|err| err.to_string())?;
                Ok(Arc::new(catalog))
            }
            #[cfg(not(feature = "offset-store-cloud-tables"))]
            {
                let _ = cfg;
                Err(
                    "skippr.store.type=cloud-tables requires --features offset-store-cloud-tables"
                        .into(),
                )
            }
        }
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
                Err(
                    "SkipprLake catalog requires offset-store-dynamodb or offset-store-cloud-tables"
                        .into(),
                )
            }
        }
    }
}

pub async fn open_iceberg_catalog(
    spec: &crate::cluster::IcebergCatalogSpec,
) -> Result<Arc<dyn iceberg::Catalog>, String> {
    open_iceberg_catalog_spec(spec).await
}

async fn open_iceberg_catalog_spec(
    spec: &crate::cluster::IcebergCatalogSpec,
) -> Result<Arc<dyn iceberg::Catalog>, String> {
    match spec {
        crate::cluster::IcebergCatalogSpec::Skippr(open) => {
            open_skippr_catalog_backend(open.backend, &open.lake).await
        }
        crate::cluster::IcebergCatalogSpec::Glue(cfg) => {
            skippr_iceberg_catalog_glue::open(&skippr_iceberg_catalog_glue::GlueOpen {
                warehouse: cfg.warehouse.clone(),
                region: cfg.region.clone(),
                catalog_id: cfg.catalog_id.clone(),
                object_store: cfg.object_store.clone(),
            })
            .await
        }
        crate::cluster::IcebergCatalogSpec::Filesystem(cfg) => {
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
    let spec: crate::cluster::IcebergCatalogSpec =
        serde_json::from_str(catalog_json).map_err(|err| err.to_string())?;
    open_iceberg_catalog_spec(&spec).await
}

pub fn offset_publisher_for(
    app_cfg: &Config,
    config: &ClusterConfig,
    key: &skippr_lease::PipelineKey,
) -> Result<crate::buffer::durable::store::OffsetMode, skippr_lease::DurableError> {
    use crate::buffer::durable::store::OffsetMode;
    use skippr_lease::DurableError;
    if uses_cloud_tables(app_cfg) {
        #[cfg(feature = "offset-store-cloud-tables")]
        {
            let store = skippr_offset_store_cloud_tables::CloudTablesOffsetStore::open(
                config.table.clone(),
                key.dynamo_pk(),
                false,
            )
            .map_err(DurableError::ProtocolMismatch)?;
            return Ok(OffsetMode::Dynamo(
                crate::buffer::durable::store::CloudOffsetPublisher::new(Arc::new(store)),
            ));
        }
        #[cfg(not(feature = "offset-store-cloud-tables"))]
        {
            let _ = (config, key);
            return Err(DurableError::ProtocolMismatch(
                "skippr.store.type=cloud-tables requires --features offset-store-cloud-tables"
                    .into(),
            ));
        }
    }
    #[cfg(feature = "offset-store-dynamodb")]
    {
        let store = skippr_offset_store_dynamodb::DynamoDbOffsetStore::open(
            config.table.clone(),
            key.dynamo_pk(),
            false,
        )
        .map_err(DurableError::ProtocolMismatch)?;
        Ok(OffsetMode::Dynamo(
            crate::buffer::durable::store::DynamoOffsetPublisher::new(Arc::new(store)),
        ))
    }
    #[cfg(not(feature = "offset-store-dynamodb"))]
    {
        let _ = (config, key);
        Err(DurableError::ProtocolMismatch(
            "clustered offset publish requires --features offset-store-dynamodb or offset-store-cloud-tables"
                .into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;

    fn disk_config() -> Config {
        serde_json::from_value(json!({
            "skippr": { "workspace": "ws-a", "tenant": "ten-a" },
            "pipelines": {
                "orders": { "data_source": "data_sources.sample" }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            }
        }))
        .unwrap()
    }

    fn yaml_offset_store(store: &str) -> Config {
        serde_json::from_value(json!({
            "skippr": { "workspace": "ws-a", "tenant": "ten-a", "offset_store": store },
            "pipelines": {
                "orders": { "data_source": "data_sources.sample" }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            }
        }))
        .unwrap()
    }

    #[test]
    fn catalog_backend_and_plugin_share_store_type() {
        let backend = include_str!("backend.rs");
        assert!(
            backend.contains("SkipprCatalogBackend::from_store_type"),
            "host catalog must use the shared selector"
        );
        assert!(
            !backend.contains(concat!("pub async fn ", "open_skippr_catalog")),
            "Skippr catalog identity is SkipprLakeOpen.backend; Config must not select the backend at query/serve open"
        );
        assert!(
            backend.contains("open_iceberg_catalog_spec"),
            "query/serve catalog open must not depend on process Config for Iceberg identity"
        );
        assert!(
            !backend.contains(concat!("open_iceberg_catalog(&", "Config::new()")),
            "Ballista reload must reopen from IcebergCatalogSpec, not Config::new()"
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
            plugin.contains("SkipprCatalogBackend::from_store_type"),
            "plugin catalog must use the shared selector"
        );
    }

    #[test]
    #[serial]
    fn yaml_cloud_tables_is_the_catalog_backend_and_plugin_env() {
        let old_store = std::env::var("SKIPPR_OFFSET_STORE").ok();
        let old_wal = std::env::var("WAL_STORAGE").ok();
        std::env::remove_var("SKIPPR_OFFSET_STORE");
        Config::set_evncache("SKIPPR_OFFSET_STORE", "");
        Config::set_wal_storage("disk");
        let config = yaml_offset_store("cloud-tables");
        assert_eq!(
            crate::pipeline_backend::offset_store_env_value(&config),
            "cloud-tables"
        );
        assert_eq!(
            catalog_backend(&config),
            skippr_iceberg_catalog::SkipprCatalogBackend::CloudTables
        );
        if let Some(value) = old_store {
            Config::set_offset_store(&value);
        } else {
            std::env::remove_var("SKIPPR_OFFSET_STORE");
            Config::set_evncache("SKIPPR_OFFSET_STORE", "");
        }
        if let Some(value) = old_wal {
            Config::set_wal_storage(&value);
        } else {
            std::env::remove_var("WAL_STORAGE");
            Config::set_evncache("WAL_STORAGE", "");
        }
    }

    #[test]
    #[serial]
    fn disk_without_offset_store_is_sled_not_cloud_tables() {
        let old_store = std::env::var("SKIPPR_OFFSET_STORE").ok();
        let old_wal = std::env::var("WAL_STORAGE").ok();
        std::env::remove_var("SKIPPR_OFFSET_STORE");
        Config::set_evncache("SKIPPR_OFFSET_STORE", "");
        Config::set_wal_storage("disk");
        let config = disk_config();
        assert_eq!(
            crate::pipeline_backend::configured_kind(&config).unwrap(),
            SkipprStoreKind::Sled
        );
        assert!(
            !uses_cloud_tables(&config),
            "Disk YAML pipelines must not switch the registry to Cloud Tables"
        );
        if let Some(value) = old_store {
            Config::set_offset_store(&value);
        } else {
            std::env::remove_var("SKIPPR_OFFSET_STORE");
            Config::set_evncache("SKIPPR_OFFSET_STORE", "");
        }
        if let Some(value) = old_wal {
            Config::set_wal_storage(&value);
        } else {
            std::env::remove_var("WAL_STORAGE");
            Config::set_evncache("WAL_STORAGE", "");
        }
    }
}
