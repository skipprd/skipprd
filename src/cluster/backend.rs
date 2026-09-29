//! Open clustered control-plane stores (DynamoDB or Cloud tables).

use std::sync::Arc;

use skippr_lease::{ClusterMembershipStore, PipelineLeaseStore};

use crate::cluster::identity::ClusterConfig;
use crate::helpers::configuration::Config;
use crate::helpers::wal_storage::OffsetStoreKind;

pub async fn open_lease_store(
    config: &Config,
    table: String,
) -> Result<Arc<dyn PipelineLeaseStore>, String> {
    let kind = crate::pipeline_backend::configured_kind(config)?;
    match kind {
        OffsetStoreKind::Sled => Err(
            "clustered pipeline leases require dynamodb or cloud-tables; sled is the Disk/S3 lease backend"
                .into(),
        ),
        OffsetStoreKind::CloudTables | OffsetStoreKind::DynamoDb => {
            crate::pipeline_backend::open_pipeline_lease_store(kind, None, table).await
        }
    }
}

pub async fn open_membership_store(
    config: &Config,
    table: String,
) -> Result<Arc<dyn ClusterMembershipStore>, String> {
    match crate::pipeline_backend::configured_kind(config)? {
        OffsetStoreKind::CloudTables => {
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
                Err("SKIPPR_OFFSET_STORE=cloud-tables requires --features offset-store-cloud-tables".into())
            }
        }
        OffsetStoreKind::DynamoDb => {
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
        OffsetStoreKind::Sled => Err(
            "clustered membership requires dynamodb or cloud-tables; sled is the Disk/S3 lease backend"
                .into(),
        ),
    }
}

pub fn uses_cloud_tables(config: &Config) -> bool {
    matches!(
        crate::pipeline_backend::configured_kind(config),
        Ok(OffsetStoreKind::CloudTables)
    )
}

pub async fn open_skippr_catalog(
    config: &Config,
    cfg: &skippr_iceberg_catalog::IcebergCatalogConfig,
) -> Result<Arc<dyn iceberg::Catalog>, String> {
    if uses_cloud_tables(config) {
        #[cfg(feature = "offset-store-cloud-tables")]
        {
            let catalog = skippr_iceberg_catalog_cloud_tables::CloudTablesCatalog::new(cfg)
                .await
                .map_err(|err| err.to_string())?;
            return Ok(Arc::new(catalog));
        }
        #[cfg(not(feature = "offset-store-cloud-tables"))]
        {
            let _ = cfg;
            return Err(
                "SKIPPR_OFFSET_STORE=cloud-tables requires --features offset-store-cloud-tables"
                    .into(),
            );
        }
    }
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
            "Skippr Iceberg catalog requires offset-store-dynamodb or offset-store-cloud-tables"
                .into(),
        )
    }
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
                "SKIPPR_OFFSET_STORE=cloud-tables requires --features offset-store-cloud-tables"
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
            OffsetStoreKind::Sled
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
