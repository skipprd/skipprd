use std::io;
use std::sync::Arc;

use iceberg::Catalog;
use skippr_iceberg_catalog::SkipprCatalogBackend;
pub use skippr_iceberg_catalog::SkipprLakeConfig;
use skippr_iceberg_catalog_dynamodb::DynamoDbCatalog;
pub use skippr_iceberg_writer::{IcebergWriter, IcebergWriterConfig};
use skippr_runtime_sdk::plugins::source_contract::SinkWritePolicySupport;

skippr_runtime_sdk::declare_sink_spec!(
    SkipprLakeSinkSpec,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::SKIPPRLAKE,
    skippr_runtime_sdk::plugins::TransactionalTableCommit
);

/// The SkipprLake data sink and schema sink: the shared writer bound to this capability.
pub type SkipprLakeWriter = IcebergWriter<SkipprLakeSinkSpec>;

const SKIPPRLAKE_WRITE_POLICIES: SinkWritePolicySupport = SinkWritePolicySupport {
    supports_merge_by_key: true,
    supports_replace_partition: true,
    supports_replace_table: true,
};

pub fn writer_config(cfg: &SkipprLakeConfig) -> IcebergWriterConfig {
    IcebergWriterConfig {
        policies: SKIPPRLAKE_WRITE_POLICIES,
        table_namespace: cfg.table_namespace.clone(),
        location_root: cfg.location_root(),
    }
}

/// The Skippr catalog: Cloud Tables when SkipprStore type selects it, else DynamoDB.
/// Host spawn injects the Config-resolved kind as `SKIPPR_STORE_TYPE`; this is the same
/// selector as `skipprd` query (`SkipprCatalogBackend::from_store_type`).
pub async fn open_catalog(cfg: &SkipprLakeConfig) -> io::Result<Arc<dyn Catalog>> {
    match SkipprCatalogBackend::from_store_type(&skippr_iceberg_catalog::store_type_from_env()) {
        SkipprCatalogBackend::CloudTables => {
            let catalog = skippr_iceberg_catalog_cloud_tables::CloudTablesCatalog::new(cfg)
                .await
                .map_err(|err| io::Error::other(err.to_string()))?;
            Ok(Arc::new(catalog))
        }
        SkipprCatalogBackend::DynamoDb => {
            let catalog = DynamoDbCatalog::new(cfg)
                .await
                .map_err(|err| io::Error::other(err.to_string()))?;
            Ok(Arc::new(catalog))
        }
    }
}

/// Construct the writer for one plugin install.
pub async fn open_writer(
    cfg: &SkipprLakeConfig,
    context: skippr_runtime_sdk::protocol::RuntimeExecutionContext,
    binding: skippr_runtime_sdk::protocol::RuntimeBinding,
    buffer_name: String,
) -> io::Result<SkipprLakeWriter> {
    let catalog = open_catalog(cfg).await?;
    SkipprLakeWriter::new(context, binding, buffer_name, catalog, writer_config(cfg)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_iceberg_catalog::WarehouseObjectStore;

    #[test]
    fn writer_config_derives_location_root_from_warehouse_and_namespace() {
        let cfg = SkipprLakeConfig {
            warehouse: "s3://lake/".into(),
            catalog_table: "cat".into(),
            region: None,
            object_store: WarehouseObjectStore::S3,
            table_namespace: "bronze".into(),
        };
        let writer = writer_config(&cfg);
        assert_eq!(writer.table_namespace, "bronze");
        assert_eq!(writer.location_root, "s3://lake/bronze");
    }
}
