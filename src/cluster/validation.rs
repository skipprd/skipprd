use crate::cli::Mode;
use crate::cluster::identity::{
    derive_advertised_ip, derive_host_id, dynamodb_route_endpoint, exclusive_data_dir_lock,
    process_generation, ClusterConfig,
};
use crate::cluster::pipeline_view::PipelineConfigView;
use crate::helpers::configuration::Config;
use crate::helpers::plugin_config::{DataSinkEntry, PluginConfigEntry};
use crate::helpers::wal_storage::{ConfigError, OffsetStoreKind, WalStorage};
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
        }
    }
}

pub fn dynamodb_feature_enabled() -> bool {
    cfg!(feature = "offset-store-dynamodb")
}

pub fn cloud_tables_feature_enabled() -> bool {
    cfg!(feature = "offset-store-cloud-tables")
}

fn cloud_tables_endpoint_configured() -> bool {
    #[cfg(feature = "offset-store-cloud-tables")]
    {
        let endpoint = std::env::var("CLOUD_TABLES_ENDPOINT").unwrap_or_default();
        return skippr_cloud::parse_tables_endpoint(&endpoint).is_ok();
    }
    #[cfg(not(feature = "offset-store-cloud-tables"))]
    false
}

fn cloud_tables_auth_configured() -> bool {
    #[cfg(feature = "offset-store-cloud-tables")]
    {
        return matches!(
            skippr_cloud::Credential::from_env(),
            Ok(skippr_cloud::Credential::Workload(_))
        );
    }
    #[cfg(not(feature = "offset-store-cloud-tables"))]
    false
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
            CliModeKind::Sync { once: false } | CliModeKind::Query | CliModeKind::SqlHelp => Ok(()),
        },
    }
}

pub fn validate_clustered_backend(
    storage: WalStorage,
    configured_offset_store: Option<OffsetStoreKind>,
    table: &str,
) -> Result<OffsetStoreKind, ConfigError> {
    match storage {
        WalStorage::Disk | WalStorage::S3 => {
            let kind = configured_offset_store.unwrap_or(OffsetStoreKind::Sled);
            if kind == OffsetStoreKind::CloudTables {
                if !cloud_tables_feature_enabled() {
                    return Err(ConfigError::ClusteredFeatureMissing);
                }
                if !cloud_tables_endpoint_configured() {
                    return Err(ConfigError::CloudTablesEndpointMissing);
                }
                if !cloud_tables_auth_configured() {
                    return Err(ConfigError::CloudTablesAuthMissing);
                }
            }
            Ok(kind)
        }
        WalStorage::Clustered => {
            if !dynamodb_feature_enabled() && !cloud_tables_feature_enabled() {
                return Err(ConfigError::ClusteredFeatureMissing);
            }
            if table.trim().is_empty() {
                return Err(ConfigError::ClusteredTableMissing);
            }
            let kind = match configured_offset_store {
                Some(OffsetStoreKind::Sled) => {
                    return Err(ConfigError::ClusteredOffsetStoreConflict("sled".into()));
                }
                Some(OffsetStoreKind::CloudTables) => OffsetStoreKind::CloudTables,
                Some(OffsetStoreKind::DynamoDb) => OffsetStoreKind::DynamoDb,
                None if dynamodb_feature_enabled() => OffsetStoreKind::DynamoDb,
                None => OffsetStoreKind::CloudTables,
            };
            match kind {
                OffsetStoreKind::DynamoDb if !dynamodb_feature_enabled() => {
                    Err(ConfigError::ClusteredFeatureMissing)
                }
                OffsetStoreKind::CloudTables if !cloud_tables_feature_enabled() => {
                    Err(ConfigError::ClusteredFeatureMissing)
                }
                OffsetStoreKind::CloudTables => {
                    if !cloud_tables_endpoint_configured() {
                        return Err(ConfigError::CloudTablesEndpointMissing);
                    }
                    if !cloud_tables_auth_configured() {
                        return Err(ConfigError::CloudTablesAuthMissing);
                    }
                    Ok(kind)
                }
                OffsetStoreKind::DynamoDb => Ok(kind),
                OffsetStoreKind::Sled => {
                    Err(ConfigError::ClusteredOffsetStoreConflict("sled".into()))
                }
            }
        }
    }
}

fn skippr_catalog_table(entry: &PluginConfigEntry) -> Option<String> {
    if !entry.plugin_name.eq_ignore_ascii_case("Iceberg") {
        return None;
    }
    let catalog = entry.config.get("catalog")?;
    let cfg: skippr_iceberg_catalog::IcebergCatalogConfig =
        serde_json::from_value(catalog.clone()).ok()?;
    cfg.skippr_table().map(str::to_string)
}

fn sink_skippr_catalog_table(entry: &DataSinkEntry) -> Option<String> {
    skippr_catalog_table(&entry.config)
}

pub fn validate_skippr_catalog_tables(
    config: &Config,
    offset_table: &str,
) -> Result<(), ConfigError> {
    let mut catalog_tables = Vec::new();
    if let Some(sinks) = &config.data_sinks {
        catalog_tables.extend(sinks.values().filter_map(sink_skippr_catalog_table));
    }
    if let Some(sinks) = &config.deadletter_sinks {
        catalog_tables.extend(sinks.values().filter_map(sink_skippr_catalog_table));
    }
    if let Some(sinks) = &config.schema_sinks {
        catalog_tables.extend(sinks.values().filter_map(skippr_catalog_table));
    }
    for catalog_table in catalog_tables {
        if skippr_iceberg_catalog::skippr_catalog_reuses_offset_table(&catalog_table, offset_table)
        {
            return Err(ConfigError::SkipprCatalogReusesOffsetTable {
                catalog_table,
                offset_table: offset_table.to_string(),
            });
        }
    }
    Ok(())
}

fn validate_configured_offset_store(
    config: &Config,
    storage: WalStorage,
) -> Result<OffsetStoreKind, ConfigError> {
    let table = config.get_offset_dynamodb_table();
    let configured = config.configured_offset_store()?;
    validate_clustered_backend(storage, configured, &table)
}

pub fn validate_clustered_cli(
    config: &Config,
    storage: WalStorage,
    mode: CliModeKind,
) -> Result<(), ConfigError> {
    validate_wal_storage_for_mode(storage, mode)?;
    match storage {
        WalStorage::Disk | WalStorage::S3 => {
            validate_configured_offset_store(config, storage)?;
            Ok(())
        }
        WalStorage::Clustered => {
            let table = config.get_offset_dynamodb_table();
            validate_configured_offset_store(config, storage)?;
            validate_skippr_catalog_tables(config, &table)?;
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
            validate_configured_offset_store(config, storage)?;
            Ok(None)
        }
        WalStorage::Clustered => {
            let table = config.get_offset_dynamodb_table();
            validate_configured_offset_store(config, storage)?;
            validate_skippr_catalog_tables(config, &table)?;
            let lock_data_dir = !matches!(mode, CliModeKind::Query | CliModeKind::SqlHelp);
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
        std::env::remove_var("SKIPPR_OFFSET_STORE");
        Config::set_offset_store("");
        assert!(validate_clustered_cli(
            &Config::new(),
            WalStorage::Disk,
            CliModeKind::Sync { once: false }
        )
        .is_ok());
    }

    #[test]
    fn clustered_without_table_fails() {
        let err = validate_clustered_backend(WalStorage::Clustered, None, "").unwrap_err();
        #[cfg(any(
            feature = "offset-store-dynamodb",
            feature = "offset-store-cloud-tables"
        ))]
        assert_eq!(err, ConfigError::ClusteredTableMissing);
        #[cfg(not(any(
            feature = "offset-store-dynamodb",
            feature = "offset-store-cloud-tables"
        )))]
        assert_eq!(err, ConfigError::ClusteredFeatureMissing);
    }

    #[test]
    fn clustered_rejects_explicit_sled() {
        let err = validate_clustered_backend(
            WalStorage::Clustered,
            Some(OffsetStoreKind::Sled),
            "offsets",
        )
        .unwrap_err();
        #[cfg(any(
            feature = "offset-store-dynamodb",
            feature = "offset-store-cloud-tables"
        ))]
        assert_eq!(
            err,
            ConfigError::ClusteredOffsetStoreConflict("sled".into())
        );
        #[cfg(not(any(
            feature = "offset-store-dynamodb",
            feature = "offset-store-cloud-tables"
        )))]
        assert_eq!(err, ConfigError::ClusteredFeatureMissing);
    }

    #[test]
    fn clustered_selects_dynamodb_when_offset_store_absent() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var("CLOUD_TABLES_ENDPOINT");
        std::env::remove_var("CLOUD_SYSTEM_BROKER_CONFIG");
        std::env::remove_var("CLOUD_SYSTEM_BROKER_ENDPOINT");
        #[cfg(feature = "offset-store-dynamodb")]
        {
            assert_eq!(
                validate_clustered_backend(WalStorage::Clustered, None, "offsets").unwrap(),
                OffsetStoreKind::DynamoDb
            );
        }
        #[cfg(all(
            feature = "offset-store-cloud-tables",
            not(feature = "offset-store-dynamodb")
        ))]
        {
            assert_eq!(
                validate_clustered_backend(WalStorage::Clustered, None, "offsets").unwrap_err(),
                ConfigError::CloudTablesEndpointMissing
            );
        }
        #[cfg(not(any(
            feature = "offset-store-dynamodb",
            feature = "offset-store-cloud-tables"
        )))]
        {
            assert_eq!(
                validate_clustered_backend(WalStorage::Clustered, None, "offsets").unwrap_err(),
                ConfigError::ClusteredFeatureMissing
            );
        }
    }

    #[test]
    fn disk_and_s3_do_not_require_dynamodb() {
        assert_eq!(
            validate_clustered_backend(WalStorage::Disk, None, "").unwrap(),
            OffsetStoreKind::Sled
        );
        assert_eq!(
            validate_clustered_backend(WalStorage::S3, Some(OffsetStoreKind::DynamoDb), "t")
                .unwrap(),
            OffsetStoreKind::DynamoDb
        );
    }

    #[cfg(feature = "offset-store-cloud-tables")]
    #[test]
    fn disk_cloud_tables_rejects_guest_held_bearer() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var("CLOUD_SYSTEM_BROKER_CONFIG");
        std::env::remove_var("CLOUD_BEARER_TOKEN");
        std::env::remove_var("CLOUD_ACCESS_KEY_ID");
        std::env::set_var("CLOUD_TABLES_ENDPOINT", "http://127.0.0.1:8003");
        std::env::set_var("CLOUD_BEARER_TOKEN", "guest-held-jwt");
        let err = validate_clustered_backend(
            WalStorage::Disk,
            Some(OffsetStoreKind::CloudTables),
            "offsets",
        )
        .unwrap_err();
        std::env::remove_var("CLOUD_TABLES_ENDPOINT");
        std::env::remove_var("CLOUD_BEARER_TOKEN");
        assert_eq!(err, ConfigError::CloudTablesAuthMissing);
    }

    #[cfg(feature = "offset-store-cloud-tables")]
    #[test]
    fn disk_cli_rejects_cloud_tables_without_workload() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var("CLOUD_SYSTEM_BROKER_CONFIG");
        std::env::remove_var("CLOUD_ACCESS_KEY_ID");
        Config::set_offset_store("cloud-tables");
        std::env::set_var("CLOUD_TABLES_ENDPOINT", "http://127.0.0.1:8003");
        std::env::set_var("CLOUD_BEARER_TOKEN", "guest-held-jwt");
        let err = validate_clustered_cli(&Config::new(), WalStorage::Disk, CliModeKind::Query)
            .unwrap_err();
        Config::set_offset_store("");
        std::env::remove_var("SKIPPR_OFFSET_STORE");
        std::env::remove_var("CLOUD_TABLES_ENDPOINT");
        std::env::remove_var("CLOUD_BEARER_TOKEN");
        assert_eq!(err, ConfigError::CloudTablesAuthMissing);
    }

    fn iceberg_skippr_sink(table: &str) -> crate::helpers::plugin_config::DataSinkEntry {
        crate::helpers::plugin_config::DataSinkEntry {
            config: crate::helpers::plugin_config::PluginConfigEntry {
                plugin_name: "Iceberg".into(),
                config: serde_json::json!({
                    "catalog": {
                        "type": "skippr",
                        "table": table,
                        "warehouse": "file:///tmp/warehouse"
                    }
                }),
            },
            schema_sink: None,
        }
    }

    #[test]
    fn skippr_catalog_table_must_not_reuse_offset_table() {
        let mut config = Config::new();
        config.data_sinks = Some(
            [("lake".into(), iceberg_skippr_sink("skippr-offsets"))]
                .into_iter()
                .collect(),
        );
        let err = validate_skippr_catalog_tables(&config, "skippr-offsets").unwrap_err();
        assert_eq!(
            err,
            ConfigError::SkipprCatalogReusesOffsetTable {
                catalog_table: "skippr-offsets".into(),
                offset_table: "skippr-offsets".into(),
            }
        );
    }

    #[cfg(feature = "offset-store-cloud-tables")]
    #[test]
    fn cloud_tables_rejects_guest_held_env_jwt() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var("CLOUD_SYSTEM_BROKER_CONFIG");
        std::env::remove_var("CLOUD_SYSTEM_BROKER_ENDPOINT");
        std::env::remove_var("CLOUD_ACCESS_KEY_ID");
        std::env::remove_var("CLOUD_OPERATOR_ACCESS_KEY_ID");
        std::env::set_var("CLOUD_TABLES_ENDPOINT", "http://127.0.0.1:8003");
        std::env::set_var("CLOUD_BEARER_TOKEN", "guest-held-jwt");
        std::env::set_var("CLOUD_TABLES_ACCESS_TOKEN", "guest-held-jwt");
        std::env::set_var("CLOUD_ACCESS_TOKEN", "guest-held-jwt");
        let err = validate_clustered_backend(
            WalStorage::Clustered,
            Some(OffsetStoreKind::CloudTables),
            "offsets",
        )
        .unwrap_err();
        std::env::remove_var("CLOUD_TABLES_ENDPOINT");
        std::env::remove_var("CLOUD_BEARER_TOKEN");
        std::env::remove_var("CLOUD_TABLES_ACCESS_TOKEN");
        std::env::remove_var("CLOUD_ACCESS_TOKEN");
        assert_eq!(err, ConfigError::CloudTablesAuthMissing);
        let msg = err.to_string();
        assert!(
            !msg.contains("CLOUD_TABLES_ACCESS_TOKEN") && !msg.contains("CLOUD_ACCESS_TOKEN"),
            "auth error must not name guest-held JWTs, got {msg}"
        );
        assert!(
            msg.contains("SDK default credentials"),
            "auth error must name SDK default credentials, got {msg}"
        );
        assert!(
            !msg.contains("GuestCredentialBroker"),
            "auth error must not name GuestCredentialBroker, got {msg}"
        );
        assert!(
            !msg.contains("CLOUD_TABLES_ENDPOINT"),
            "auth error must not claim the endpoint is missing when it is present, got {msg}"
        );
    }

    #[cfg(feature = "offset-store-cloud-tables")]
    #[test]
    fn cloud_tables_rejects_non_mesh_endpoint() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::set_var("CLOUD_TABLES_ENDPOINT", "http://example.com");
        std::env::remove_var("CLOUD_SYSTEM_BROKER_CONFIG");
        let err = validate_clustered_backend(
            WalStorage::Clustered,
            Some(OffsetStoreKind::CloudTables),
            "offsets",
        )
        .unwrap_err();
        std::env::remove_var("CLOUD_TABLES_ENDPOINT");
        assert_eq!(err, ConfigError::CloudTablesEndpointMissing);
    }

    #[cfg(feature = "offset-store-cloud-tables")]
    #[test]
    fn cloud_tables_accepts_broker_config_drive() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("skipprd-broker-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("broker.json"),
            r#"{
  "profile": "skippr",
  "guest_id": "skippr-0",
  "cluster_generation": 1,
  "guest_incarnation": "inc-1",
  "broker_endpoint": "http://127.0.0.1:8097",
  "capability": "opaque"
}
"#,
        )
        .unwrap();
        std::env::set_var("CLOUD_TABLES_ENDPOINT", "http://127.0.0.1:8003");
        std::env::set_var("CLOUD_SYSTEM_BROKER_CONFIG", &dir);
        std::env::remove_var("CLOUD_TABLES_ACCESS_TOKEN");
        std::env::remove_var("CLOUD_ACCESS_TOKEN");
        std::env::remove_var("CLOUD_BEARER_TOKEN");
        std::env::remove_var("CLOUD_ACCESS_KEY_ID");
        std::env::remove_var("CLOUD_OPERATOR_ACCESS_KEY_ID");
        let kind = validate_clustered_backend(
            WalStorage::Clustered,
            Some(OffsetStoreKind::CloudTables),
            "offsets",
        );
        std::env::remove_var("CLOUD_TABLES_ENDPOINT");
        std::env::remove_var("CLOUD_SYSTEM_BROKER_CONFIG");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(kind.unwrap(), OffsetStoreKind::CloudTables);
    }

    #[test]
    fn skippr_catalog_table_may_differ_from_offset_table() {
        let mut config = Config::new();
        config.data_sinks = Some(
            [("lake".into(), iceberg_skippr_sink("skippr-iceberg-catalog"))]
                .into_iter()
                .collect(),
        );
        assert!(validate_skippr_catalog_tables(&config, "skippr-offsets").is_ok());
    }
}
