use crate::cluster::{PipelineConfigView, QueryBackend};
use crate::discover::PipelineMetadata;
use crate::helpers::configuration::Config;
use crate::sqlrt::parser::TableDropStatement;
use skippr_iceberg_catalog::SkipprLakeConfig;

/// Removes a table from the metadata and drops the SkipprLake Iceberg table.
pub async fn drop_table(
    config: &Config,
    pipeline_metadata: &mut PipelineMetadata,
    stmt: &TableDropStatement,
) -> Result<(), String> {
    let table_str = format!("{}", stmt.table);
    let pipeline = match &stmt.schema {
        Some(schema) => format!("{}", schema),
        None => table_str.clone(),
    };

    pipeline_metadata.metadata.remove(&table_str);

    let ns = &table_str;
    let view = PipelineConfigView::for_name(config, &pipeline).map_err(|err| err.to_string())?;
    match &view.backend {
        QueryBackend::SkipprLake(cfg) => {
            drop_skipprlake_table(config, cfg, ns).await?;
        }
        QueryBackend::WalOnly => {
            return Err("DROP TABLE is only supported for SkipprLake pipelines".into());
        }
    }

    {
        let tenant = config.get_tenant();
        let workspace = config.get_workspace_name();
        let prefix_root = format!("{}/{}/{}", tenant, workspace, pipeline);
        if !tenant.is_empty()
            && !workspace.is_empty()
            && !pipeline.is_empty()
            && !prefix_root.starts_with('/')
            && prefix_root.contains('/')
        {
            let storage = crate::adapters::storage::get_storage(config);
            if !ns.is_empty() && ns != "/" {
                let _ = storage
                    .delete_prefix(&format!("{}/stats/{}", prefix_root, ns))
                    .await;
                let _ = storage
                    .delete_prefix(&format!("{}/semantic/{}", prefix_root, ns))
                    .await;
                let _ = storage
                    .delete_prefix(&format!("{}/catalog/{}", prefix_root, ns))
                    .await;
            }
            let _ = storage
                .delete_prefix(&format!("{}/wal/", prefix_root))
                .await;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::sql::sqlparser::ast::{Ident, ObjectName};

    fn config_with_file_sink() -> Config {
        let mut sink = serde_json::Map::new();
        sink.insert("File".to_string(), serde_json::json!({"path": "/tmp/out"}));
        serde_json::from_value(serde_json::json!({
            "skippr": { "workspace": "ws", "tenant": "t" },
            "pipelines": {
                "p": {
                    "data_source": "data_sources.sample",
                    "data_sink": "data_sinks.out"
                }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            },
            "data_sinks": {
                "out": sink
            }
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn wal_only_drop_table_is_rejected() {
        let config = config_with_file_sink().bind_pipeline("p");
        let mut metadata = PipelineMetadata::new(&config);
        let stmt = TableDropStatement {
            schema: Some(ObjectName::from(vec![Ident::new("p")])),
            table: ObjectName::from(vec![Ident::new("ns")]),
        };
        let err = drop_table(&config, &mut metadata, &stmt).await.unwrap_err();
        assert!(
            err.contains("DROP TABLE is only supported for SkipprLake pipelines"),
            "{err}"
        );
    }

    #[test]
    fn drop_table_does_not_purge_parquet_layout() {
        let src = include_str!("drop_table.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            !prod.contains("/parquet/"),
            "query must not purge warehouse data; data purge stays a sink concern"
        );
        assert!(prod.contains("drop_skipprlake_table"));
        assert!(prod.contains("/wal/"));
    }
}

/// Drops a SkipprLake Iceberg namespace. Other backends are rejected.
pub async fn drop_database(config: &Config, database: &str) -> Result<(), String> {
    let mut matched: Option<SkipprLakeConfig> = None;
    for name in config.pipelines.keys() {
        let view = PipelineConfigView::for_name(config, name).map_err(|err| err.to_string())?;
        match &view.backend {
            QueryBackend::SkipprLake(cfg) if cfg.table_namespace == database => {
                if matched.is_some() {
                    return Err(format!(
                        "DROP DATABASE '{database}' matches multiple SkipprLake pipelines"
                    ));
                }
                matched = Some(cfg.clone());
            }
            QueryBackend::SkipprLake(_) | QueryBackend::WalOnly => {}
        }
    }
    let Some(cfg) = matched else {
        return Err("DROP DATABASE is only supported for SkipprLake pipelines".into());
    };
    drop_skipprlake_namespace(config, &cfg).await
}

async fn drop_skipprlake_table(
    config: &Config,
    cfg: &SkipprLakeConfig,
    table: &str,
) -> Result<(), String> {
    #[cfg(any(
        feature = "offset-store-dynamodb",
        feature = "offset-store-cloud-tables"
    ))]
    {
        let catalog = crate::cluster::backend::open_skippr_catalog(config, cfg).await?;
        let ident = iceberg::TableIdent::from_strs([cfg.table_namespace.as_str(), table])
            .map_err(|err| err.to_string())?;
        iceberg::Catalog::drop_table(catalog.as_ref(), &ident)
            .await
            .map_err(|err| err.to_string())
    }
    #[cfg(not(any(
        feature = "offset-store-dynamodb",
        feature = "offset-store-cloud-tables"
    )))]
    {
        let _ = (config, cfg, table);
        Err(
            "SkipprLake DROP TABLE requires offset-store-dynamodb or offset-store-cloud-tables"
                .into(),
        )
    }
}

async fn drop_skipprlake_namespace(config: &Config, cfg: &SkipprLakeConfig) -> Result<(), String> {
    #[cfg(any(
        feature = "offset-store-dynamodb",
        feature = "offset-store-cloud-tables"
    ))]
    {
        let catalog = crate::cluster::backend::open_skippr_catalog(config, cfg).await?;
        let ns = iceberg::NamespaceIdent::from_strs([&cfg.table_namespace])
            .map_err(|err| err.to_string())?;
        iceberg::Catalog::drop_namespace(catalog.as_ref(), &ns)
            .await
            .map_err(|err| err.to_string())
    }
    #[cfg(not(any(
        feature = "offset-store-dynamodb",
        feature = "offset-store-cloud-tables"
    )))]
    {
        let _ = (config, cfg);
        Err(
            "SkipprLake DROP DATABASE requires offset-store-dynamodb or offset-store-cloud-tables"
                .into(),
        )
    }
}
