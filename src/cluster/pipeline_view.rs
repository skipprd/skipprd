use std::path::PathBuf;

use crate::buffer::compaction_transaction::SinkRetrySemantics;
use crate::cluster::iceberg_lake::{FsCatalogConfig, GlueCatalogConfig, IcebergLake};
use crate::connect::DataSink;
use crate::helpers::configuration::{Config, Pipeline, Registry};
use crate::helpers::wal_storage::{ConfigError, WalStorage};
use crate::plugins::cdc::SinkCapability;
use skippr_iceberg_catalog::SkipprLakeConfig;
use skippr_lease::{PipelineKey, PipelinePaths};

/// How `skipprd query` reads one pipeline. Resolved once at `PipelineConfigView::for_name`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueryBackend {
    /// Iceberg data sink: Iceberg catalog ∪ live WAL.
    Iceberg(IcebergLake),
    /// Non-Iceberg sink, or no sink: live WAL only (local query). Clustered: typed notice, not registered.
    WalOnly,
}

impl QueryBackend {
    pub fn for_pipeline(config: &Config, pipeline: &str) -> Result<Self, ConfigError> {
        let pipeline_cfg = config
            .pipelines
            .get(pipeline)
            .ok_or_else(|| ConfigError::PipelineNotFound(pipeline.to_string()))?;
        let Some(data_sink_ref) = pipeline_cfg.data_sink.as_ref() else {
            return Ok(Self::WalOnly);
        };
        let name = Config::parse_registry_ref(data_sink_ref, Registry::DataSinks)
            .map_err(ConfigError::InvalidIdentity)?;
        let Some(entry) = config
            .data_sinks
            .as_ref()
            .and_then(|sinks| sinks.get(&name))
        else {
            return Err(ConfigError::InvalidIdentity(format!(
                "data_sink '{name}' is not defined"
            )));
        };
        match DataSink::parse(&entry.config.plugin_name) {
            Some(DataSink::SkipprLake) => {
                let cfg: SkipprLakeConfig = entry
                    .config
                    .deserialize()
                    .map_err(|err| ConfigError::IcebergConfigInvalid(err))?;
                cfg.validate().map_err(ConfigError::IcebergConfigInvalid)?;
                let backend = skippr_iceberg_catalog::SkipprCatalogBackend::from_store_type(
                    &crate::pipeline_backend::skippr_store_type_value(config),
                );
                Ok(Self::Iceberg(IcebergLake::skippr(cfg, backend)))
            }
            Some(DataSink::AthenaIceberg) => {
                let cfg: GlueCatalogConfig = entry
                    .config
                    .deserialize()
                    .map_err(|err| ConfigError::IcebergConfigInvalid(err))?;
                if cfg.warehouse.trim().is_empty() || cfg.glue_database_name.trim().is_empty() {
                    return Err(ConfigError::IcebergConfigInvalid(
                        "AthenaIceberg warehouse and glue_database_name are required".into(),
                    ));
                }
                skippr_iceberg_catalog::require_s3_warehouse(&cfg.warehouse)
                    .map_err(ConfigError::IcebergConfigInvalid)?;
                Ok(Self::Iceberg(IcebergLake::glue(cfg)))
            }
            Some(DataSink::Duckdb) => {
                let cfg: FsCatalogConfig = entry
                    .config
                    .deserialize()
                    .map_err(|err| ConfigError::IcebergConfigInvalid(err))?;
                skippr_iceberg_catalog::validate_file_warehouse(
                    &cfg.warehouse,
                    &cfg.table_namespace,
                )
                .map_err(ConfigError::IcebergConfigInvalid)?;
                Ok(Self::Iceberg(IcebergLake::filesystem(cfg)))
            }
            _ => Ok(Self::WalOnly),
        }
    }
}

/// Immutable per-pipeline view. Replica/query/scheduler code uses this instead of
/// process-global pipeline identity.
#[derive(Clone, Debug)]
pub struct PipelineConfigView {
    pub key: PipelineKey,
    pub data_root: PathBuf,
    pub source_plugin: String,
    pub sink_plugin: String,
    pub schema_plugin: Option<String>,
    pub sink_ref: Option<String>,
    pub backend: QueryBackend,
    pub flatten_events: bool,
    pub wal_storage: WalStorage,
}

impl PipelineConfigView {
    pub fn for_name(config: &Config, pipeline: &str) -> Result<Self, ConfigError> {
        let pipeline_cfg = config
            .pipelines
            .get(pipeline)
            .ok_or_else(|| ConfigError::PipelineNotFound(pipeline.to_string()))?;
        let tenant = tenant_from_config(config);
        let workspace = workspace_from_config(config);
        let key = PipelineKey::new(tenant, workspace, pipeline)
            .map_err(|err| ConfigError::InvalidIdentity(err.to_string()))?;
        let data_root = data_root_for_pipeline(config, pipeline_cfg);
        let (sink_ref, sink_plugin, schema_plugin) = resolve_sink(config, pipeline_cfg);
        let source_plugin = resolve_source(config, pipeline_cfg);
        let backend = QueryBackend::for_pipeline(config, pipeline)?;
        let flatten_events = config
            .bind_pipeline(pipeline)
            .get_transform_flatten_events();
        Ok(Self {
            key,
            data_root,
            source_plugin,
            sink_plugin,
            schema_plugin,
            sink_ref,
            backend,
            flatten_events,
            wal_storage: config.get_wal_storage(),
        })
    }

    pub fn for_registry(config: &Config, key: &PipelineKey) -> Result<Self, ConfigError> {
        if !config.pipelines.contains_key(key.pipeline()) {
            return Err(ConfigError::PipelineNotFound(key.pipeline().to_string()));
        }
        let mut view = Self::for_name(config, key.pipeline())?;
        view.key = key.clone();
        Ok(view)
    }

    pub fn key(&self) -> &PipelineKey {
        &self.key
    }

    pub fn paths(&self) -> Result<PipelinePaths, ConfigError> {
        PipelinePaths::new(&self.data_root, &self.key)
            .map_err(|err| ConfigError::InvalidIdentity(err.to_string()))
    }

    /// WAL segs for `skipprd query` when no durable store is installed.
    /// Layout follows `WAL_STORAGE` (`WalStorage`), not directory existence.
    pub fn query_wal_paths(&self) -> Result<Option<PipelinePaths>, ConfigError> {
        match self.wal_storage {
            WalStorage::Clustered => self.paths().map(Some),
            WalStorage::Disk => {
                Ok(Some(PipelinePaths::legacy_disk(&self.data_root.join(
                    format!("{}_{}", self.key.workspace(), self.key.pipeline()),
                ))))
            }
            WalStorage::S3 => Ok(None),
        }
    }

    pub fn sink_capability(&self) -> Option<&'static SinkCapability> {
        crate::plugins::cdc::sink_capabilities::by_name(&self.sink_plugin)
    }

    pub fn validate_clustered_sink(&self) -> Result<(), ConfigError> {
        let Some(capability) = self.sink_capability() else {
            return Err(ConfigError::ClusteredSinkNotIdempotent {
                plugin: self.sink_plugin.clone(),
                semantics: "unknown".into(),
            });
        };
        if !capability.retry_semantics.requires_idempotent_replay() {
            return Err(ConfigError::ClusteredSinkNotIdempotent {
                plugin: capability.name.to_string(),
                semantics: format!("{:?}", capability.retry_semantics),
            });
        }
        if capability.grouping_support.is_none() {
            return Err(ConfigError::ClusteredSinkGroupingUnsupported {
                plugin: capability.name.to_string(),
            });
        }
        let _ = SinkRetrySemantics::equals(capability.retry_semantics, capability.retry_semantics);
        Ok(())
    }
}

fn tenant_from_config(config: &Config) -> String {
    config
        .skippr
        .as_ref()
        .and_then(|s| s.tenant.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::env::var("TENANT").unwrap_or_else(|_| "default".into()))
}

fn workspace_from_config(config: &Config) -> String {
    config
        .skippr
        .as_ref()
        .and_then(|s| s.workspace.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::env::var("WORKSPACE_NAME").unwrap_or_else(|_| "default".into()))
}

fn data_root_for_pipeline(config: &Config, pipeline: &Pipeline) -> PathBuf {
    let default = std::env::var("DATA_DIR").unwrap_or_else(|_| "./data".into());
    let dir = pipeline
        .data_dir
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| config.skippr.as_ref().and_then(|_| None))
        .unwrap_or(default);
    PathBuf::from(dir)
}

fn resolve_source(config: &Config, pipeline: &Pipeline) -> String {
    let Some(data_source_ref) = pipeline.data_source.as_ref() else {
        return String::new();
    };
    let name = Config::parse_registry_ref(data_source_ref, Registry::DataSources)
        .unwrap_or_else(|_| data_source_ref.clone());
    config
        .data_sources
        .as_ref()
        .and_then(|sources| sources.get(&name))
        .and_then(|entry| entry.plugin_name())
        .unwrap_or_default()
}

fn resolve_sink(config: &Config, pipeline: &Pipeline) -> (Option<String>, String, Option<String>) {
    let Some(data_sink_ref) = pipeline.data_sink.as_ref() else {
        return (None, String::new(), None);
    };
    let name = Config::parse_registry_ref(data_sink_ref, Registry::DataSinks)
        .unwrap_or_else(|_| data_sink_ref.clone());
    match config
        .data_sinks
        .as_ref()
        .and_then(|sinks| sinks.get(&name))
    {
        Some(entry) => (
            Some(data_sink_ref.clone()),
            entry.config.plugin_name().unwrap_or_default(),
            entry.schema_sink.clone(),
        ),
        None => (Some(data_sink_ref.clone()), String::new(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clustered_rejects_at_least_once_sink() {
        let view = PipelineConfigView {
            key: PipelineKey::new("t", "w", "p").unwrap(),
            data_root: PathBuf::from("/tmp"),
            source_plugin: "S3".into(),
            sink_plugin: "Stdout".into(),
            schema_plugin: None,
            sink_ref: None,
            backend: QueryBackend::WalOnly,
            flatten_events: false,
            wal_storage: WalStorage::Disk,
        };
        assert!(view.validate_clustered_sink().is_err());
        assert_eq!(
            crate::plugins::cdc::sink_capabilities::by_name("Stdout")
                .unwrap()
                .name,
            "Stdout"
        );
    }

    #[test]
    fn clustered_accepts_skipprlake() {
        let view = PipelineConfigView {
            key: PipelineKey::new("t", "w", "p").unwrap(),
            data_root: PathBuf::from("/tmp"),
            source_plugin: "S3".into(),
            sink_plugin: skippr_iceberg_catalog::SkipprLakeConfig::PLUGIN_NAME.into(),
            schema_plugin: None,
            sink_ref: None,
            backend: QueryBackend::Iceberg(IcebergLake::skippr(
                SkipprLakeConfig {
                    warehouse: "file:///tmp/warehouse".into(),
                    catalog_table: "cat".into(),
                    region: None,
                    object_store: skippr_iceberg_catalog::WarehouseObjectStore::File,
                    table_namespace: "default".into(),
                },
                skippr_iceberg_catalog::SkipprCatalogBackend::DynamoDb,
            )),
            flatten_events: false,
            wal_storage: WalStorage::Clustered,
        };
        assert!(view.validate_clustered_sink().is_ok());
    }

    #[test]
    fn clustered_accepts_athena_iceberg() {
        let view = PipelineConfigView {
            key: PipelineKey::new("t", "w", "p").unwrap(),
            data_root: PathBuf::from("/tmp"),
            source_plugin: "S3".into(),
            sink_plugin: crate::plugins::cdc::sink_capabilities::ATHENA_ICEBERG
                .name
                .into(),
            schema_plugin: None,
            sink_ref: None,
            backend: QueryBackend::Iceberg(IcebergLake::glue(GlueCatalogConfig {
                warehouse: "s3://wh/".into(),
                glue_database_name: "db".into(),
                region: None,
                catalog_id: None,
                object_store: skippr_iceberg_catalog::S3CompatibleObjectStore::S3,
            })),
            flatten_events: false,
            wal_storage: WalStorage::Clustered,
        };
        assert!(view.validate_clustered_sink().is_ok());
        assert!(matches!(view.backend, QueryBackend::Iceberg(_)));
    }

    #[test]
    fn clustered_accepts_duckdb() {
        let view = PipelineConfigView {
            key: PipelineKey::new("t", "w", "p").unwrap(),
            data_root: PathBuf::from("/tmp"),
            source_plugin: "File".into(),
            sink_plugin: crate::plugins::cdc::sink_capabilities::DUCKDB.name.into(),
            schema_plugin: None,
            sink_ref: None,
            backend: QueryBackend::Iceberg(IcebergLake::filesystem(FsCatalogConfig {
                warehouse: "file:///tmp/lake".into(),
                table_namespace: "bronze".into(),
            })),
            flatten_events: false,
            wal_storage: WalStorage::Clustered,
        };
        assert!(view.validate_clustered_sink().is_ok());
        assert!(matches!(view.backend, QueryBackend::Iceberg(_)));
    }

    #[test]
    fn registry_without_yaml_fails_closed() {
        let config = Config::new();
        let key = PipelineKey::new("system", "platform", "otel-logs").unwrap();
        let err = PipelineConfigView::for_registry(&config, &key).unwrap_err();
        assert!(matches!(err, ConfigError::PipelineNotFound(_)));
    }

    #[test]
    fn query_wal_paths_follow_wal_storage() {
        let dir = tempfile::tempdir().unwrap();
        let data_root = dir.path();
        let key = PipelineKey::new("default", "de-query-r2", "orders_el").unwrap();
        let disk = PipelineConfigView {
            key: key.clone(),
            data_root: data_root.to_path_buf(),
            source_plugin: "File".into(),
            sink_plugin: String::new(),
            schema_plugin: None,
            sink_ref: None,
            backend: QueryBackend::WalOnly,
            flatten_events: false,
            wal_storage: WalStorage::Disk,
        };
        let disk_root = data_root.join("de-query-r2_orders_el");
        let paths = disk.query_wal_paths().unwrap().unwrap();
        assert_eq!(paths.segs, disk_root.join("segment_buffer/segs"));

        let clustered = PipelineConfigView {
            wal_storage: WalStorage::Clustered,
            ..disk.clone()
        };
        let clustered_paths = clustered.query_wal_paths().unwrap().unwrap();
        assert!(clustered_paths
            .root
            .ends_with("clustered/default/de-query-r2/orders_el"));

        let s3 = PipelineConfigView {
            wal_storage: WalStorage::S3,
            ..disk
        };
        assert!(s3.query_wal_paths().unwrap().is_none());
    }

    fn config_with_sink(plugin: &str, body: serde_json::Value) -> Config {
        let mut sink = serde_json::Map::new();
        sink.insert(plugin.to_string(), body);
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

    #[test]
    fn athena_sink_resolves_wal_only() {
        let cfg = config_with_sink(
            "Athena",
            serde_json::json!({
                "s3_bucket": "b",
                "s3_prefix": "p",
                "athena_workgroup_name": "wg",
                "athena_results_s3_bucket": "r"
            }),
        );
        assert_eq!(
            QueryBackend::for_pipeline(&cfg, "p").unwrap(),
            QueryBackend::WalOnly
        );
    }

    #[test]
    fn skipprlake_sink_resolves_typed_backend() {
        let cfg = config_with_sink(
            "SkipprLake",
            serde_json::json!({
                "warehouse": "s3://wh/",
                "catalog_table": "cat",
                "region": "us-east-1",
                "table_namespace": "bronze"
            }),
        );
        assert!(matches!(
            QueryBackend::for_pipeline(&cfg, "p").unwrap(),
            QueryBackend::Iceberg(_)
        ));
        match QueryBackend::for_pipeline(&cfg, "p").unwrap() {
            QueryBackend::Iceberg(lake) => match lake.catalog {
                crate::cluster::IcebergCatalogSpec::Skippr(open) => {
                    assert_eq!(
                        open.backend,
                        skippr_iceberg_catalog::SkipprCatalogBackend::DynamoDb
                    );
                }
                other => panic!("expected Skippr catalog, got {other:?}"),
            },
            other => panic!("expected Iceberg, got {other:?}"),
        }
    }

    #[test]
    fn malformed_skipprlake_config_fails_closed_not_wal_only() {
        let cfg = config_with_sink(
            "SkipprLake",
            serde_json::json!({"warehouse": "s3://wh/", "catalog": {"type": "glue"}}),
        );
        assert!(QueryBackend::for_pipeline(&cfg, "p").is_err());
    }

    #[test]
    fn skipprlake_file_warehouse_requires_file_object_store() {
        let missing = config_with_sink(
            "SkipprLake",
            serde_json::json!({
                "warehouse": "file:///tmp/warehouse",
                "catalog_table": "cat",
                "table_namespace": "bronze"
            }),
        );
        let err = QueryBackend::for_pipeline(&missing, "p").unwrap_err();
        assert!(err.to_string().contains("s3://"), "{err}");
        let cfg = config_with_sink(
            "SkipprLake",
            serde_json::json!({
                "warehouse": "file:///tmp/warehouse",
                "catalog_table": "cat",
                "object_store": { "type": "file" },
                "table_namespace": "bronze"
            }),
        );
        match QueryBackend::for_pipeline(&cfg, "p").unwrap() {
            QueryBackend::Iceberg(lake) => {
                assert_eq!(lake.ingest_namespace, "bronze");
            }
            other => panic!("expected Iceberg, got {other:?}"),
        }
    }

    #[test]
    fn athena_iceberg_rejects_file_warehouse() {
        let cfg = config_with_sink(
            "AthenaIceberg",
            serde_json::json!({
                "warehouse": "file:///tmp/lake",
                "glue_database_name": "db",
                "athena_workgroup_name": "wg",
                "athena_results_s3_bucket": "r"
            }),
        );
        let err = QueryBackend::for_pipeline(&cfg, "p").unwrap_err();
        assert!(err.to_string().contains("s3://"), "{err}");
    }

    #[test]
    fn athena_iceberg_cannot_name_file_object_store() {
        let cfg = config_with_sink(
            "AthenaIceberg",
            serde_json::json!({
                "warehouse": "s3://wh/",
                "glue_database_name": "db",
                "athena_workgroup_name": "wg",
                "athena_results_s3_bucket": "r",
                "object_store": { "type": "file" }
            }),
        );
        assert!(QueryBackend::for_pipeline(&cfg, "p").is_err());
    }

    #[test]
    fn athena_iceberg_sink_resolves_iceberg() {
        let cfg = config_with_sink(
            "AthenaIceberg",
            serde_json::json!({
                "warehouse": "s3://wh/",
                "glue_database_name": "db",
                "athena_workgroup_name": "wg",
                "athena_results_s3_bucket": "r"
            }),
        );
        match QueryBackend::for_pipeline(&cfg, "p").unwrap() {
            QueryBackend::Iceberg(lake) => {
                assert_eq!(lake.ingest_namespace, "db");
                assert_eq!(lake.catalog.kind_name(), "AthenaIceberg");
            }
            other => panic!("expected Iceberg, got {other:?}"),
        }
    }

    #[test]
    fn duckdb_sink_resolves_iceberg() {
        let cfg = config_with_sink(
            "Duckdb",
            serde_json::json!({
                "warehouse": "file:///tmp/lake",
                "table_namespace": "bronze"
            }),
        );
        match QueryBackend::for_pipeline(&cfg, "p").unwrap() {
            QueryBackend::Iceberg(lake) => {
                assert_eq!(lake.ingest_namespace, "bronze");
                assert_eq!(lake.catalog.kind_name(), "Duckdb");
            }
            other => panic!("expected Iceberg, got {other:?}"),
        }
    }

    #[test]
    fn malformed_athena_iceberg_config_fails_closed_not_wal_only() {
        let cfg = config_with_sink(
            "AthenaIceberg",
            serde_json::json!({"warehouse": "s3://wh/"}),
        );
        assert!(QueryBackend::for_pipeline(&cfg, "p").is_err());
    }

    #[test]
    fn malformed_duckdb_config_fails_closed_not_wal_only() {
        let cfg = config_with_sink(
            "Duckdb",
            serde_json::json!({"warehouse": "s3://not-file", "table_namespace": "bronze"}),
        );
        assert!(QueryBackend::for_pipeline(&cfg, "p").is_err());
    }

    #[test]
    fn missing_data_sink_entry_fails_closed_not_wal_only() {
        let cfg = serde_json::from_value(serde_json::json!({
            "skippr": { "workspace": "ws", "tenant": "t" },
            "pipelines": {
                "p": {
                    "data_source": "data_sources.sample",
                    "data_sink": "data_sinks.missing"
                }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            },
            "data_sinks": {}
        }))
        .unwrap();
        let err = QueryBackend::for_pipeline(&cfg, "p").unwrap_err();
        assert!(matches!(err, ConfigError::InvalidIdentity(_)), "{err:?}");
    }

    #[test]
    fn malformed_data_sink_ref_fails_closed_not_wal_only() {
        let cfg = serde_json::from_value(serde_json::json!({
            "skippr": { "workspace": "ws", "tenant": "t" },
            "pipelines": {
                "p": {
                    "data_source": "data_sources.sample",
                    "data_sink": "not-a-registry-ref"
                }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            }
        }))
        .unwrap();
        let err = QueryBackend::for_pipeline(&cfg, "p").unwrap_err();
        assert!(matches!(err, ConfigError::InvalidIdentity(_)), "{err:?}");
    }
}
