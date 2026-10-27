use crate::cli::Mode;
use crate::cluster::identity::{
    derive_advertised_ip, derive_host_id, dynamodb_route_endpoint, exclusive_data_dir_lock,
    process_generation, ClusterConfig,
};
use crate::cluster::pipeline_view::PipelineConfigView;
use crate::helpers::configuration::Config;
use crate::helpers::plugin_config::{DataSinkEntry, PluginConfigEntry};
use crate::helpers::wal_storage::{ConfigError, SkipprStoreKind, WalStorage};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliModeKind {
    Discover,
    Metadata,
    Sync { once: bool },
    Query,
    Schema,
    SqlHelp,
    Benchmark,
    Connect,
    Serve,
    Migrate,
}

impl CliModeKind {
    pub fn from_mode(mode: &Mode) -> Self {
        match mode {
            Mode::Discover(_) => Self::Discover,
            Mode::Metadata { .. } => Self::Metadata,
            Mode::Sync(opts) => Self::Sync { once: opts.once },
            Mode::Query(_) => Self::Query,
            Mode::Schema(_) => Self::Schema,
            Mode::SqlHelp(_) => Self::SqlHelp,
            Mode::Benchmark(_) => Self::Benchmark,
            Mode::Doctor(_) => Self::SqlHelp,
            Mode::Df(_) => Self::Query,
            Mode::Connect(_) => Self::Connect,
            Mode::Serve(_) => Self::Serve,
            Mode::Migrate(_) => Self::Migrate,
        }
    }

    pub fn as_label(self) -> &'static str {
        match self {
            Self::Discover => "discover",
            Self::Metadata => "metadata",
            Self::Sync { once: true } => "sync --once",
            Self::Sync { once: false } => "sync",
            Self::Query => "query",
            Self::Schema => "schema",
            Self::SqlHelp => "sql-help",
            Self::Benchmark => "benchmark",
            Self::Connect => "connect",
            Self::Serve => "serve",
            Self::Migrate => "migrate",
        }
    }
}

pub fn dynamodb_feature_enabled() -> bool {
    cfg!(feature = "offset-store-dynamodb")
}

pub fn validate_wal_storage_for_mode(
    storage: WalStorage,
    mode: CliModeKind,
) -> Result<(), ConfigError> {
    match storage {
        WalStorage::Disk | WalStorage::S3 => Ok(()),
        WalStorage::Clustered => match mode {
            CliModeKind::Sync { once: true } => Err(ConfigError::ClusteredOnceRejected),
            CliModeKind::Discover => Err(ConfigError::ClusteredModeRejected("discover".into())),
            CliModeKind::Metadata => Err(ConfigError::ClusteredModeRejected("metadata".into())),
            CliModeKind::Benchmark => Err(ConfigError::ClusteredModeRejected("benchmark".into())),
            CliModeKind::Schema => Err(ConfigError::ClusteredModeRejected("schema".into())),
            CliModeKind::Serve => Err(ConfigError::ClusteredModeRejected("serve".into())),
            CliModeKind::Sync { once: false }
            | CliModeKind::Query
            | CliModeKind::SqlHelp
            | CliModeKind::Connect
            | CliModeKind::Migrate => Ok(()),
        },
    }
}

pub fn validate_clustered_backend(
    storage: WalStorage,
    kind: SkipprStoreKind,
    table: &str,
) -> Result<SkipprStoreKind, ConfigError> {
    match storage {
        WalStorage::Disk | WalStorage::S3 => Ok(kind),
        WalStorage::Clustered => {
            if !dynamodb_feature_enabled() {
                return Err(ConfigError::ClusteredFeatureMissing);
            }
            if table.trim().is_empty() {
                return Err(ConfigError::ClusteredTableMissing);
            }
            match kind {
                SkipprStoreKind::DynamoDb => Ok(kind),
                SkipprStoreKind::Sled => Err(ConfigError::ClusteredStoreConflict("sled".into())),
            }
        }
    }
}

fn skippr_catalog_table(entry: &PluginConfigEntry) -> Result<Option<String>, ConfigError> {
    if crate::connect::DataSink::parse(&entry.plugin_name)
        != Some(crate::connect::DataSink::SkipprLake)
        && crate::connect::SchemaSink::parse(&entry.plugin_name)
            != Some(crate::connect::SchemaSink::SkipprLake)
    {
        return Ok(None);
    }
    let cfg: skippr_iceberg_catalog::SkipprLakeConfig =
        serde_json::from_value(entry.config.clone())
            .map_err(|err| ConfigError::IcebergConfigInvalid(err.to_string()))?;
    Ok(Some(cfg.catalog_table))
}

fn sink_skippr_catalog_table(entry: &DataSinkEntry) -> Result<Option<String>, ConfigError> {
    skippr_catalog_table(&entry.config)
}

pub fn validate_skippr_catalog_tables(config: &Config) -> Result<(), ConfigError> {
    if let Some(sinks) = &config.data_sinks {
        for entry in sinks.values() {
            let _ = sink_skippr_catalog_table(entry)?;
        }
    }
    if let Some(sinks) = &config.deadletter_sinks {
        for entry in sinks.values() {
            let _ = sink_skippr_catalog_table(entry)?;
        }
    }
    if let Some(sinks) = &config.schema_sinks {
        for entry in sinks.values() {
            let _ = skippr_catalog_table(entry)?;
        }
    }
    Ok(())
}

fn validate_configured_skippr_store(
    config: &Config,
    storage: WalStorage,
) -> Result<SkipprStoreKind, ConfigError> {
    let table = config.get_skippr_store_name();
    let kind = crate::pipeline_backend::configured_kind(config)
        .map_err(ConfigError::InvalidSkipprStore)?;
    validate_clustered_backend(storage, kind, &table)
}

pub fn validate_clustered_cli(
    config: &Config,
    storage: WalStorage,
    mode: CliModeKind,
) -> Result<(), ConfigError> {
    validate_wal_storage_for_mode(storage, mode)?;
    match storage {
        WalStorage::Disk | WalStorage::S3 => {
            validate_configured_skippr_store(config, storage)?;
            Ok(())
        }
        WalStorage::Clustered => {
            validate_configured_skippr_store(config, storage)?;
            validate_skippr_catalog_tables(config)?;
            if matches!(mode, CliModeKind::Sync { once: false }) {
                for name in config.pipelines.keys() {
                    PipelineConfigView::for_name(config, name)?.validate_clustered_sink()?;
                }
            }
            Ok(())
        }
    }
}

pub fn validate_clustered_mode(
    config: &Config,
    storage: WalStorage,
    mode: CliModeKind,
) -> Result<Option<ClusterConfig>, ConfigError> {
    validate_wal_storage_for_mode(storage, mode)?;
    match storage {
        WalStorage::Disk | WalStorage::S3 => {
            validate_configured_skippr_store(config, storage)?;
            Ok(None)
        }
        WalStorage::Clustered => {
            let table = config.get_skippr_store_name();
            validate_configured_skippr_store(config, storage)?;
            validate_skippr_catalog_tables(config)?;
            let lock_data_dir = !matches!(
                mode,
                CliModeKind::Query | CliModeKind::SqlHelp | CliModeKind::Connect
            );
            if lock_data_dir {
                for name in config.pipelines.keys() {
                    PipelineConfigView::for_name(config, name)?.validate_clustered_sink()?;
                }
            }
            let data_root = PathBuf::from(config.get_pipeline_data_dir());
            if lock_data_dir {
                let _lock = exclusive_data_dir_lock(&data_root)?;
                std::mem::forget(_lock);
            }
            let cluster_id = std::env::var("SKIPPR_CLUSTER_ID")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .ok_or(ConfigError::ClusterIdMissing)?;
            let cluster_id = skippr_lease::ClusterId::new(cluster_id)
                .map_err(|err| ConfigError::InvalidIdentity(err.to_string()))?;
            let gossip_hmac_key = std::env::var("SKIPPR_CLUSTER_GOSSIP_KEY")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .ok_or(ConfigError::GossipKeyMissing)?
                .into_bytes();
            if std::env::var("SKIPPR_CLUSTER_TLS_CERT")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .is_none()
                || std::env::var("SKIPPR_CLUSTER_TLS_KEY")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .is_none()
                || std::env::var("SKIPPR_CLUSTER_TLS_CA")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .is_none()
            {
                return Err(ConfigError::ClusterTlsMissing);
            }
            Ok(Some(ClusterConfig {
                storage,
                table,
                cluster_id,
                gossip_hmac_key,
                node_id: process_generation(),
                host_id: derive_host_id()?,
                data_root,
                advertised_ip: derive_advertised_ip(&dynamodb_route_endpoint())?,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn clustered_once_is_rejected() {
        let err =
            validate_wal_storage_for_mode(WalStorage::Clustered, CliModeKind::Sync { once: true })
                .unwrap_err();
        assert_eq!(err, ConfigError::ClusteredOnceRejected);
    }

    #[test]
    fn clustered_discover_is_rejected() {
        assert!(matches!(
            validate_wal_storage_for_mode(WalStorage::Clustered, CliModeKind::Discover),
            Err(ConfigError::ClusteredModeRejected(_))
        ));
    }

    #[test]
    fn clustered_query_is_allowed() {
        assert!(validate_wal_storage_for_mode(WalStorage::Clustered, CliModeKind::Query).is_ok());
    }

    #[test]
    fn clustered_cli_validation_does_not_require_data_dir_lock() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SKIPPR_STORE_TYPE");
        Config::set_skippr_store_type("");
        assert!(validate_clustered_cli(
            &Config::new(),
            WalStorage::Disk,
            CliModeKind::Sync { once: false }
        )
        .is_ok());
    }

    #[test]
    fn clustered_without_table_fails() {
        let err = validate_clustered_backend(
            WalStorage::Clustered,
            SkipprStoreKind::default_for_wal(WalStorage::Clustered),
            "",
        )
        .unwrap_err();
        #[cfg(feature = "offset-store-dynamodb")]
        assert_eq!(err, ConfigError::ClusteredTableMissing);
        #[cfg(not(feature = "offset-store-dynamodb"))]
        assert_eq!(err, ConfigError::ClusteredFeatureMissing);
    }

    #[test]
    fn clustered_rejects_explicit_sled() {
        let err =
            validate_clustered_backend(WalStorage::Clustered, SkipprStoreKind::Sled, "offsets")
                .unwrap_err();
        #[cfg(feature = "offset-store-dynamodb")]
        assert_eq!(err, ConfigError::ClusteredStoreConflict("sled".into()));
        #[cfg(not(feature = "offset-store-dynamodb"))]
        assert_eq!(err, ConfigError::ClusteredFeatureMissing);
    }

    #[test]
    fn clustered_selects_dynamodb_when_offset_store_absent() {
        #[cfg(feature = "offset-store-dynamodb")]
        {
            assert_eq!(
                validate_clustered_backend(
                    WalStorage::Clustered,
                    SkipprStoreKind::default_for_wal(WalStorage::Clustered),
                    "offsets"
                )
                .unwrap(),
                SkipprStoreKind::DynamoDb
            );
        }
        #[cfg(not(feature = "offset-store-dynamodb"))]
        {
            assert_eq!(
                validate_clustered_backend(
                    WalStorage::Clustered,
                    SkipprStoreKind::default_for_wal(WalStorage::Clustered),
                    "offsets"
                )
                .unwrap_err(),
                ConfigError::ClusteredFeatureMissing
            );
        }
    }

    #[test]
    fn disk_and_s3_do_not_require_dynamodb() {
        assert_eq!(
            validate_clustered_backend(
                WalStorage::Disk,
                SkipprStoreKind::default_for_wal(WalStorage::Disk),
                ""
            )
            .unwrap(),
            SkipprStoreKind::Sled
        );
        assert_eq!(
            validate_clustered_backend(WalStorage::S3, SkipprStoreKind::DynamoDb, "t").unwrap(),
            SkipprStoreKind::DynamoDb
        );
    }

    fn iceberg_skippr_sink(table: &str) -> crate::helpers::plugin_config::DataSinkEntry {
        crate::helpers::plugin_config::DataSinkEntry {
            config: crate::helpers::plugin_config::PluginConfigEntry {
                plugin_name: skippr_iceberg_catalog::SkipprLakeConfig::PLUGIN_NAME.into(),
                config: serde_json::json!({
                    "catalog_table": table,
                    "warehouse": "file:///tmp/warehouse",
                    "object_store": { "type": "file" },
                    "table_namespace": "default"
                }),
            },
            schema_sink: None,
        }
    }

    #[test]
    fn skippr_catalog_table_may_reuse_store_table() {
        let mut config = Config::new();
        config.data_sinks = Some(
            [("lake".into(), iceberg_skippr_sink("skippr-offsets"))]
                .into_iter()
                .collect(),
        );
        assert!(validate_skippr_catalog_tables(&config).is_ok());
    }

    #[test]
    fn skippr_catalog_table_undecodable_config_fails_closed() {
        let mut config = Config::new();
        config.data_sinks = Some(
            [(
                "lake".into(),
                crate::helpers::plugin_config::DataSinkEntry {
                    config: crate::helpers::plugin_config::PluginConfigEntry {
                        plugin_name: skippr_iceberg_catalog::SkipprLakeConfig::PLUGIN_NAME.into(),
                        config: serde_json::json!({ "catalog_table": "cat" }),
                    },
                    schema_sink: None,
                },
            )]
            .into_iter()
            .collect(),
        );
        let err = validate_skippr_catalog_tables(&config).unwrap_err();
        assert!(
            matches!(err, ConfigError::IcebergConfigInvalid(_)),
            "{err:?}"
        );
    }

    #[test]
    fn skippr_catalog_table_may_differ_from_offset_table() {
        let mut config = Config::new();
        config.data_sinks = Some(
            [("lake".into(), iceberg_skippr_sink("skippr-iceberg-catalog"))]
                .into_iter()
                .collect(),
        );
        assert!(validate_skippr_catalog_tables(&config).is_ok());
    }
}
