use std::io;
use std::sync::Arc;

use iceberg::Catalog;
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

/// SkipprLake catalog pointers live in DynamoDB.
pub async fn open_catalog(cfg: &SkipprLakeConfig) -> io::Result<Arc<dyn Catalog>> {
    let catalog = DynamoDbCatalog::new(cfg)
        .await
        .map_err(|err| io::Error::other(err.to_string()))?;
    Ok(Arc::new(catalog))
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

    fn region_metadata() -> skippr_runtime_sdk::discover::OutputMetadata {
        serde_json::from_value(serde_json::json!({
            "out_field_name": "",
            "determined_type": "record",
            "determined_type_values": "",
            "fields": {
                "region": {
                    "out_field_name": "region",
                    "determined_type": "string",
                    "determined_type_values": "",
                    "field_id": 7,
                    "schema_id": 1,
                    "lineage_id": "orders:region",
                    "nullable": true,
                    "default_value": null,
                    "fields": {}
                }
            }
        }))
        .unwrap()
    }

    fn runtime_context() -> skippr_runtime_sdk::protocol::RuntimeExecutionContext {
        skippr_runtime_sdk::protocol::RuntimeExecutionContext {
            pipeline_name: "p".into(),
            workspace_name: "w".into(),
            data_dir: "/tmp".into(),
            execution_mode: Default::default(),
            output_layout: Default::default(),
            inject_fields: std::collections::BTreeMap::new(),
            pipeline_type: Default::default(),
        }
    }

    #[tokio::test]
    async fn apply_alter_rename_keeps_field_id_in_memory_catalog() {
        use std::collections::HashMap;
        use std::sync::Arc;

        use iceberg::memory::{MemoryCatalogBuilder, MEMORY_CATALOG_WAREHOUSE};
        use iceberg::{Catalog, CatalogBuilder, NamespaceIdent, TableIdent};
        use skippr_runtime_sdk::discover::schema_alter::SchemaAlterOp;
        use skippr_runtime_sdk::plugins::{AlterSupport, SchemaSink};

        let catalog: Arc<dyn Catalog> = Arc::new(
            MemoryCatalogBuilder::default()
                .load(
                    "test",
                    HashMap::from([(
                        MEMORY_CATALOG_WAREHOUSE.to_string(),
                        "memory://warehouse-skipprlake-alter".to_string(),
                    )]),
                )
                .await
                .unwrap(),
        );
        catalog
            .create_namespace(
                &NamespaceIdent::from_strs(["lake"]).unwrap(),
                HashMap::new(),
            )
            .await
            .unwrap();
        let cfg = SkipprLakeConfig {
            warehouse: "memory://warehouse-skipprlake-alter".into(),
            catalog_table: "cat".into(),
            region: None,
            object_store: WarehouseObjectStore::S3,
            table_namespace: "lake".into(),
        };
        let writer = SkipprLakeWriter::new(
            runtime_context(),
            skippr_runtime_sdk::protocol::RuntimeBinding::Primary,
            "schema".into(),
            Arc::clone(&catalog),
            writer_config(&cfg),
        )
        .await
        .unwrap();
        assert_eq!(writer.alter_support(), AlterSupport::Native);

        let metadata = region_metadata();
        writer.sync_schema("orders", &metadata).await.unwrap();
        let ident = TableIdent::from_strs(["lake", "orders"]).unwrap();
        let created = catalog.load_table(&ident).await.unwrap();
        let field_id = created
            .metadata()
            .current_schema()
            .field_by_name("region")
            .unwrap()
            .id;
        writer
            .apply_alter(
                "orders",
                &metadata,
                &SchemaAlterOp::Rename {
                    from: "region".into(),
                    to: "region_code".into(),
                },
                field_id,
            )
            .await
            .unwrap();

        let loaded = catalog.load_table(&ident).await.unwrap();
        let schema = loaded.metadata().current_schema();
        assert_eq!(schema.field_by_name("region_code").unwrap().id, field_id);
        assert!(schema.field_by_name("region").is_none());
    }
}
