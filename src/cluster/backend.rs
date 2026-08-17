//! Open clustered control-plane stores (DynamoDB or Cloud tables).

use std::sync::Arc;

use skippr_lease::{ClusterMembershipStore, PipelineLeaseStore};

use crate::cluster::identity::ClusterConfig;
use crate::helpers::configuration::Config;
use crate::helpers::wal_storage::OffsetStoreKind;

pub async fn open_lease_store(table: String) -> Result<Arc<dyn PipelineLeaseStore>, String> {
    match configured_kind()? {
        OffsetStoreKind::CloudTables => {
            #[cfg(feature = "offset-store-cloud-tables")]
            {
                let store = skippr_store_cloud_tables::CloudTablesLeaseStore::connect(table)
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
        OffsetStoreKind::DynamoDb | OffsetStoreKind::Sled => {
            #[cfg(feature = "offset-store-dynamodb")]
            {
                let store = skippr_lease_store_dynamodb::DynamoDbLeaseStore::connect(table)
                    .await
                    .map_err(|err| err.to_string())?;
                Ok(Arc::new(store))
            }
            #[cfg(not(feature = "offset-store-dynamodb"))]
            {
                let _ = table;
                Err("clustered sync requires --features offset-store-dynamodb".into())
            }
        }
    }
}

pub async fn open_membership_store(
    table: String,
) -> Result<Arc<dyn ClusterMembershipStore>, String> {
    match configured_kind()? {
        OffsetStoreKind::CloudTables => {
            #[cfg(feature = "offset-store-cloud-tables")]
            {
                let store = skippr_store_cloud_tables::CloudTablesMembershipStore::connect(table)
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
        OffsetStoreKind::DynamoDb | OffsetStoreKind::Sled => {
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
    }
}

pub fn configured_kind() -> Result<OffsetStoreKind, String> {
    match Config::configured_offset_store() {
        Ok(Some(kind)) => Ok(kind),
        Ok(None) => {
            if cfg!(feature = "offset-store-dynamodb") {
                Ok(OffsetStoreKind::DynamoDb)
            } else {
                Ok(OffsetStoreKind::CloudTables)
            }
        }
        Err(err) => Err(err.to_string()),
    }
}

pub fn uses_cloud_tables() -> bool {
    matches!(configured_kind(), Ok(OffsetStoreKind::CloudTables))
}

pub async fn open_skippr_catalog(
    cfg: &skippr_iceberg_catalog::IcebergCatalogConfig,
) -> Result<Arc<dyn iceberg::Catalog>, String> {
    if uses_cloud_tables() {
        #[cfg(feature = "offset-store-cloud-tables")]
        {
            let catalog = skippr_store_cloud_tables::CloudTablesCatalog::new(cfg)
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
    config: &ClusterConfig,
    key: &skippr_lease::PipelineKey,
) -> Result<crate::buffer::durable::store::OffsetMode, skippr_lease::DurableError> {
    use crate::buffer::durable::store::OffsetMode;
    use skippr_lease::DurableError;
    if uses_cloud_tables() {
        #[cfg(feature = "offset-store-cloud-tables")]
        {
            let store = skippr_store_cloud_tables::CloudTablesOffsetStore::open(
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
        Ok(OffsetMode::Dynamo(
            crate::buffer::durable::store::MemoryOffsetPublisher::new(),
        ))
    }
}
