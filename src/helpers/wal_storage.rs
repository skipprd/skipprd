use clap::ValueEnum;
use serde_derive::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// WAL backend selected by `WAL_STORAGE` / `--wal-storage`.
///
/// Unknown values are startup errors. There is no silent fallthrough to disk.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum WalStorage {
    #[default]
    Disk,
    S3,
    Clustered,
}

impl WalStorage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disk => "disk",
            Self::S3 => "s3",
            Self::Clustered => "clustered",
        }
    }

    pub fn requires_cluster_services(self) -> bool {
        match self {
            Self::Clustered => true,
            Self::Disk | Self::S3 => false,
        }
    }

    pub fn uses_local_disk_segments(self) -> bool {
        match self {
            Self::Disk | Self::Clustered => true,
            Self::S3 => false,
        }
    }
}

impl fmt::Display for WalStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WalStorage {
    type Err = ConfigError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "disk" => Ok(Self::Disk),
            "s3" => Ok(Self::S3),
            "clustered" => Ok(Self::Clustered),
            other => Err(ConfigError::InvalidWalStorage(other.to_owned())),
        }
    }
}

/// Extract/load metadata storage selected by `skippr.skipprd_el_storage_mode`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ElStorageMode {
    Local,
    #[default]
    S3,
}

impl ElStorageMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::S3 => "s3",
        }
    }
}

impl fmt::Display for ElStorageMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ElStorageMode {
    type Err = ConfigError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "s3" | "" => Ok(Self::S3),
            other => Err(ConfigError::InvalidElStorageMode(other.to_owned())),
        }
    }
}

/// SkipprStore backend selected by `skippr.store.type` / `SKIPPR_STORE_TYPE`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, Deserialize, Serialize)]
pub enum SkipprStoreKind {
    #[default]
    #[serde(rename = "sled")]
    #[value(name = "sled")]
    Sled,
    #[serde(rename = "dynamodb")]
    #[value(name = "dynamodb")]
    DynamoDb,
    #[serde(rename = "cloud-tables", alias = "cloud_tables", alias = "tables")]
    #[value(name = "cloud-tables")]
    CloudTables,
}

/// Durable Skippr KV store: offsets, checkpoints, leases, membership, and
/// SkipprLake catalog pointers share one table via non-colliding PK/SK prefixes.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct SkipprStore {
    #[serde(rename = "type")]
    pub kind: SkipprStoreKind,
    /// Table name for DynamoDB / Cloud Tables. Unused for sled.
    #[serde(default)]
    pub name: Option<String>,
}

impl SkipprStoreKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sled => "sled",
            Self::DynamoDb => "dynamodb",
            Self::CloudTables => "cloud-tables",
        }
    }

    pub fn is_clustered_control_plane(self) -> bool {
        matches!(self, Self::DynamoDb | Self::CloudTables)
    }

    /// Disk/S3 default to sled. Clustered defaults to DynamoDB when that
    /// feature is on, otherwise Cloud Tables.
    pub fn default_for_wal(storage: WalStorage) -> Self {
        match storage {
            WalStorage::Clustered => {
                if cfg!(feature = "offset-store-dynamodb") {
                    Self::DynamoDb
                } else {
                    Self::CloudTables
                }
            }
            WalStorage::Disk | WalStorage::S3 => Self::Sled,
        }
    }
}

impl FromStr for SkipprStoreKind {
    type Err = ConfigError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let raw = raw.trim().to_ascii_lowercase();
        if skippr_iceberg_catalog::store_is_cloud_tables(&raw) {
            return Ok(Self::CloudTables);
        }
        match raw.as_str() {
            "" | "sled" => Ok(Self::Sled),
            "dynamodb" => Ok(Self::DynamoDb),
            other => Err(ConfigError::InvalidSkipprStore(other.to_owned())),
        }
    }
}

/// Deprecated: `SKIPPR_OFFSET_STORE` / `skippr.offset_store`. Use `SkipprStoreKind`.
pub type OffsetStoreKind = SkipprStoreKind;

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid WAL_STORAGE value '{0}'; expected disk, s3, or clustered")]
    InvalidWalStorage(String),
    #[error("invalid skipprd_el_storage_mode value '{0}'; expected local or s3")]
    InvalidElStorageMode(String),
    #[error("invalid skippr.store.type value '{0}'; expected sled, dynamodb, or cloud-tables")]
    InvalidSkipprStore(String),
    #[error("WAL_STORAGE=clustered requires skipprd built with --features offset-store-dynamodb or offset-store-cloud-tables")]
    ClusteredFeatureMissing,
    #[error("WAL_STORAGE=clustered requires skippr.store.name (or SKIPPR_STORE_NAME)")]
    ClusteredTableMissing,
    #[error("skippr.store.type=cloud-tables requires CLOUD_TABLES_ENDPOINT (mesh/loopback, not *.cloud.skippr.io)")]
    CloudTablesEndpointMissing,
    #[error("Cloud Tables SkipprStore requires SDK default credentials")]
    CloudTablesAuthMissing,
    #[error("WAL_STORAGE=clustered requires SKIPPR_CLUSTER_ID")]
    ClusterIdMissing,
    #[error("WAL_STORAGE=clustered requires SKIPPR_CLUSTER_GOSSIP_KEY")]
    GossipKeyMissing,
    #[error("WAL_STORAGE=clustered requires SKIPPR_CLUSTER_TLS_CERT, SKIPPR_CLUSTER_TLS_KEY, and SKIPPR_CLUSTER_TLS_CA")]
    ClusterTlsMissing,
    #[error("SkipprLake config is invalid: {0}")]
    SkipprLakeConfigInvalid(String),
    #[error(
        "WAL_STORAGE=clustered cannot be used with skippr.store.type={0}; clustered mode uses DynamoDB or Cloud tables"
    )]
    ClusteredOffsetStoreConflict(String),
    #[error(
        "WAL_STORAGE=clustered does not support sync --once; a quorum member must remain available"
    )]
    ClusteredOnceRejected,
    #[error("WAL_STORAGE=clustered does not support {0}")]
    ClusteredModeRejected(String),
    #[error(
        "clustered sink '{plugin}' retry_semantics={semantics} does not support idempotent replay"
    )]
    ClusteredSinkNotIdempotent { plugin: String, semantics: String },
    #[error("clustered sink '{plugin}' does not declare grouped idempotency/preflight support")]
    ClusteredSinkGroupingUnsupported { plugin: String },
    #[error("pipeline '{0}' not found")]
    PipelineNotFound(String),
    #[error("clustered process lock failed for DATA_DIR '{path}': {detail}")]
    ClusteredDataDirLock { path: String, detail: String },
    #[error("{0}")]
    InvalidIdentity(String),
    #[error("{0}")]
    InvalidAdvertisedAddress(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_wal_storage_value() {
        assert_eq!("disk".parse::<WalStorage>().unwrap(), WalStorage::Disk);
        assert_eq!("S3".parse::<WalStorage>().unwrap(), WalStorage::S3);
        assert_eq!(
            " clustered ".parse::<WalStorage>().unwrap(),
            WalStorage::Clustered
        );
    }

    #[test]
    fn unknown_wal_storage_is_an_error() {
        match "memory".parse::<WalStorage>() {
            Err(ConfigError::InvalidWalStorage(value)) => assert_eq!(value, "memory"),
            other => panic!("unexpected parse result: {other:?}"),
        }
    }

    #[test]
    fn parses_el_storage_mode() {
        assert_eq!(
            "local".parse::<ElStorageMode>().unwrap(),
            ElStorageMode::Local
        );
        assert_eq!("S3".parse::<ElStorageMode>().unwrap(), ElStorageMode::S3);
        assert!("memory".parse::<ElStorageMode>().is_err());
    }

    #[test]
    fn wal_storage_has_no_silent_default_for_unknown_values() {
        assert!("foo".parse::<WalStorage>().is_err());
        assert_eq!(WalStorage::default(), WalStorage::Disk);
    }

    #[test]
    fn src_has_no_stringly_wal_storage_s3_branches() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        for entry in walkdir::WalkDir::new(&root) {
            let entry = entry.unwrap();
            if entry.path().extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(entry.path()).unwrap();
            if text.contains("get_wal_storage().eq_ignore_ascii_case(\"s3\")") {
                offenders.push(entry.path().display().to_string());
            }
        }
        assert!(
            offenders.is_empty(),
            "WAL_STORAGE must be matched as WalStorage, found: {offenders:?}"
        );
    }

    #[test]
    fn default_for_wal_is_the_store_kind_resolver() {
        assert_eq!(
            SkipprStoreKind::default_for_wal(WalStorage::Disk),
            SkipprStoreKind::Sled
        );
        assert_eq!(
            SkipprStoreKind::default_for_wal(WalStorage::S3),
            SkipprStoreKind::Sled
        );
        #[cfg(feature = "offset-store-dynamodb")]
        assert_eq!(
            SkipprStoreKind::default_for_wal(WalStorage::Clustered),
            SkipprStoreKind::DynamoDb
        );
        #[cfg(all(
            feature = "offset-store-cloud-tables",
            not(feature = "offset-store-dynamodb")
        ))]
        assert_eq!(
            SkipprStoreKind::default_for_wal(WalStorage::Clustered),
            SkipprStoreKind::CloudTables
        );
    }

    #[test]
    fn cloud_tables_store_aliases_parse() {
        assert_eq!(
            "cloud-tables".parse::<SkipprStoreKind>().unwrap(),
            SkipprStoreKind::CloudTables
        );
        assert_eq!(
            "tables".parse::<SkipprStoreKind>().unwrap(),
            SkipprStoreKind::CloudTables
        );
        assert_eq!(
            "CLOUD_TABLES".parse::<SkipprStoreKind>().unwrap(),
            SkipprStoreKind::CloudTables
        );
        assert_eq!(
            "dynamodb".parse::<SkipprStoreKind>().unwrap(),
            SkipprStoreKind::DynamoDb
        );
    }

    #[test]
    fn skippr_store_yaml_is_type_and_name() {
        let store: SkipprStore = serde_yaml::from_str("type: dynamodb\nname: offsets\n").unwrap();
        assert_eq!(store.kind, SkipprStoreKind::DynamoDb);
        assert_eq!(store.name.as_deref(), Some("offsets"));
    }
}
