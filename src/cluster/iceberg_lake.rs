use serde_derive::{Deserialize, Serialize};
#[cfg(test)]
use skippr_iceberg_catalog::WarehouseObjectStore;
use skippr_iceberg_catalog::{S3CompatibleObjectStore, SkipprCatalogBackend, SkipprLakeConfig};

/// Serializable Iceberg catalog identity for query, DDL, Ballista reload, and serve.
/// Opened asynchronously via [`crate::cluster::backend::open_iceberg_catalog`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IcebergCatalogSpec {
    Skippr(SkipprLakeOpen),
    Glue(GlueCatalogConfig),
    Filesystem(FsCatalogConfig),
}

/// Skippr catalog pointer store is part of lake identity (Ballista reload must not guess DynamoDB).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SkipprLakeOpen {
    pub lake: SkipprLakeConfig,
    pub backend: SkipprCatalogBackend,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GlueCatalogConfig {
    pub warehouse: String,
    pub glue_database_name: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub catalog_id: Option<String>,
    #[serde(default)]
    pub object_store: S3CompatibleObjectStore,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FsCatalogConfig {
    pub warehouse: String,
    pub table_namespace: String,
}

/// One skipprd Iceberg sink lake: catalog identity plus the ingest namespace the writer owns.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IcebergLake {
    pub catalog: IcebergCatalogSpec,
    pub ingest_namespace: String,
}

fn backend_key(backend: SkipprCatalogBackend) -> &'static str {
    match backend {
        SkipprCatalogBackend::DynamoDb => "dynamodb",
        SkipprCatalogBackend::CloudTables => "cloud-tables",
    }
}

impl IcebergCatalogSpec {
    /// Same physical catalog can hold several ingest namespaces (one serve, one REST).
    pub fn physical_key(&self) -> String {
        match self {
            Self::Skippr(open) => format!(
                "skippr\0{}\0{}\0{}\0{}\0{}",
                skippr_iceberg_catalog::warehouse_key(&open.lake.warehouse),
                open.lake.catalog_table,
                open.lake.region.as_deref().unwrap_or(""),
                backend_key(open.backend),
                open.lake.object_store.physical_key()
            ),
            Self::Glue(cfg) => format!(
                "glue\0{}\0{}\0{}\0{}",
                skippr_iceberg_catalog::warehouse_key(&cfg.warehouse),
                cfg.catalog_id.as_deref().unwrap_or(""),
                cfg.region.as_deref().unwrap_or(""),
                cfg.object_store.physical_key()
            ),
            Self::Filesystem(cfg) => format!(
                "fs\0{}",
                skippr_iceberg_catalog::warehouse_key(&cfg.warehouse)
            ),
        }
    }

    pub fn warehouse(&self) -> &str {
        match self {
            Self::Skippr(open) => &open.lake.warehouse,
            Self::Glue(cfg) => &cfg.warehouse,
            Self::Filesystem(cfg) => &cfg.warehouse,
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Skippr(_) => "SkipprLake",
            Self::Glue(_) => "AthenaIceberg",
            Self::Filesystem(_) => "Duckdb",
        }
    }
}

impl IcebergLake {
    pub fn skippr(cfg: SkipprLakeConfig, backend: SkipprCatalogBackend) -> Self {
        let ingest_namespace = cfg.table_namespace.clone();
        Self {
            catalog: IcebergCatalogSpec::Skippr(SkipprLakeOpen { lake: cfg, backend }),
            ingest_namespace,
        }
    }

    pub fn glue(cfg: GlueCatalogConfig) -> Self {
        let ingest_namespace = cfg.glue_database_name.clone();
        Self {
            catalog: IcebergCatalogSpec::Glue(cfg),
            ingest_namespace,
        }
    }

    pub fn filesystem(cfg: FsCatalogConfig) -> Self {
        let ingest_namespace = cfg.table_namespace.clone();
        Self {
            catalog: IcebergCatalogSpec::Filesystem(cfg),
            ingest_namespace,
        }
    }
}

/// `skipprd serve` exposes one physical Iceberg catalog. Mixed Skippr/Glue/filesystem is illegal.
pub fn require_one_physical_catalog(lakes: &[IcebergLake]) -> Result<&IcebergCatalogSpec, String> {
    let mut iter = lakes.iter();
    let Some(first) = iter.next() else {
        return Err("serve requires at least one Iceberg sink".into());
    };
    let key = first.catalog.physical_key();
    for lake in iter {
        if lake.catalog.physical_key() != key {
            return Err(format!(
                "serve exposes one Iceberg catalog; found {} and {}",
                first.catalog.kind_name(),
                lake.catalog.kind_name()
            ));
        }
    }
    Ok(&first.catalog)
}

/// Iceberg namespaces Flight SQL exposes as `namespace.table`.
///
/// Ingest namespaces are included so dbt-skipprlake can `source()` Iceberg idents
/// (`bronze.shop`) while skipprd query still aliases ingest as `pipeline.table`.
/// Pipeline names remain illegal as Iceberg namespaces.
pub fn extra_namespace_names(
    listed: impl IntoIterator<Item = String>,
    pipelines: &std::collections::BTreeSet<String>,
) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for name in listed {
        if pipelines.contains(&name) {
            return Err(format!(
                "Iceberg namespace '{name}' collides with pipeline '{name}'"
            ));
        }
        out.push(name);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skippr_cfg(ns: &str) -> SkipprLakeConfig {
        SkipprLakeConfig {
            warehouse: "s3://wh/".into(),
            catalog_table: "cat".into(),
            region: None,
            object_store: WarehouseObjectStore::S3,
            table_namespace: ns.into(),
        }
    }

    fn skippr_lake(ns: &str) -> IcebergLake {
        IcebergLake::skippr(skippr_cfg(ns), SkipprCatalogBackend::DynamoDb)
    }

    #[test]
    fn skippr_lakes_same_warehouse_share_physical_key() {
        let a = IcebergLake::skippr(
            SkipprLakeConfig {
                warehouse: "s3://wh/".into(),
                catalog_table: "cat".into(),
                region: Some("us-east-1".into()),
                object_store: WarehouseObjectStore::S3,
                table_namespace: "bronze".into(),
            },
            SkipprCatalogBackend::DynamoDb,
        );
        let b = IcebergLake::skippr(
            SkipprLakeConfig {
                warehouse: "s3://wh".into(),
                catalog_table: "cat".into(),
                region: Some("us-east-1".into()),
                object_store: WarehouseObjectStore::S3,
                table_namespace: "silver".into(),
            },
            SkipprCatalogBackend::DynamoDb,
        );
        assert_eq!(a.catalog.physical_key(), b.catalog.physical_key());
        assert_ne!(a.ingest_namespace, b.ingest_namespace);
    }

    #[test]
    fn skippr_physical_key_includes_backend_and_object_store() {
        let dynamo = IcebergLake::skippr(skippr_cfg("bronze"), SkipprCatalogBackend::DynamoDb);
        let cloud = IcebergLake::skippr(skippr_cfg("bronze"), SkipprCatalogBackend::CloudTables);
        assert_ne!(dynamo.catalog.physical_key(), cloud.catalog.physical_key());
        let mut r2 = skippr_cfg("bronze");
        r2.object_store = WarehouseObjectStore::R2 {
            endpoint: "https://r2.example".into(),
            region: Some("auto".into()),
            access_key_id: "AKIAEXAMPLEKEY".into(),
            secret_access_key: "wJalrXUtnFEMIsecret".into(),
            path_style: true,
        };
        let r2_lake = IcebergLake::skippr(r2, SkipprCatalogBackend::DynamoDb);
        assert_ne!(
            dynamo.catalog.physical_key(),
            r2_lake.catalog.physical_key()
        );
        assert!(r2_lake
            .catalog
            .physical_key()
            .contains("r2:https://r2.example"));
        assert!(!r2_lake.catalog.physical_key().contains("AKIAEXAMPLEKEY"));
        assert!(!r2_lake
            .catalog
            .physical_key()
            .contains("wJalrXUtnFEMIsecret"));
    }

    #[test]
    fn glue_and_skippr_are_distinct_physical_catalogs() {
        let skippr = IcebergCatalogSpec::Skippr(SkipprLakeOpen {
            lake: skippr_cfg("bronze"),
            backend: SkipprCatalogBackend::DynamoDb,
        });
        let glue = IcebergCatalogSpec::Glue(GlueCatalogConfig {
            warehouse: "s3://wh/".into(),
            glue_database_name: "bronze".into(),
            region: None,
            catalog_id: None,
            object_store: S3CompatibleObjectStore::S3,
        });
        assert_ne!(skippr.physical_key(), glue.physical_key());
    }

    #[test]
    fn serve_rejects_mixed_physical_catalogs() {
        let skippr = skippr_lake("bronze");
        let glue = IcebergLake::glue(GlueCatalogConfig {
            warehouse: "s3://wh/".into(),
            glue_database_name: "bronze".into(),
            region: None,
            catalog_id: None,
            object_store: S3CompatibleObjectStore::S3,
        });
        let err = require_one_physical_catalog(&[skippr, glue]).unwrap_err();
        assert!(err.contains("one Iceberg catalog"), "{err}");
        assert!(err.contains("SkipprLake"), "{err}");
        assert!(err.contains("AthenaIceberg"), "{err}");
    }

    #[test]
    fn serve_accepts_two_namespaces_on_one_skippr_catalog() {
        assert!(
            require_one_physical_catalog(&[skippr_lake("bronze"), skippr_lake("silver")]).is_ok()
        );
    }

    #[test]
    fn serve_rejects_empty_iceberg_set() {
        let err = require_one_physical_catalog(&[]).unwrap_err();
        assert!(err.contains("at least one Iceberg sink"), "{err}");
    }

    #[test]
    fn extra_namespace_names_keeps_ingest_and_rejects_pipeline_collision() {
        let pipelines = ["shop".into()].into_iter().collect();
        assert_eq!(
            extra_namespace_names(["bronze".into(), "analytics".into()], &pipelines).unwrap(),
            vec!["bronze".to_string(), "analytics".to_string()]
        );
        let err = extra_namespace_names(["shop".into()], &pipelines).unwrap_err();
        assert!(err.contains("collides with pipeline"), "{err}");
    }
}
