use clap::ValueEnum;
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

/// Offset/checkpoint backend selected by `SKIPPR_OFFSET_STORE`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OffsetStoreKind {
    #[default]
    Sled,
    DynamoDb,
    CloudTables,
}

impl OffsetStoreKind {
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
}

impl FromStr for OffsetStoreKind {
    type Err = ConfigError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "sled" => Ok(Self::Sled),
            "dynamodb" => Ok(Self::DynamoDb),
            "cloud-tables" | "cloud_tables" | "tables" => Ok(Self::CloudTables),
            other => Err(ConfigError::InvalidOffsetStore(other.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid WAL_STORAGE value '{0}'; expected disk, s3, or clustered")]
    InvalidWalStorage(String),
    #[error("invalid SKIPPR_OFFSET_STORE value '{0}'; expected sled, dynamodb, or cloud-tables")]
    InvalidOffsetStore(String),
    #[error("WAL_STORAGE=clustered requires skipprd built with --features offset-store-dynamodb or offset-store-cloud-tables")]
    ClusteredFeatureMissing,
    #[error("WAL_STORAGE=clustered requires SKIPPR_OFFSET_DYNAMODB_TABLE")]
    ClusteredTableMissing,
    #[error("SKIPPR_OFFSET_STORE=cloud-tables requires CLOUD_TABLES_ENDPOINT (mesh/loopback, not *.cloud.skippr.io)")]
    CloudTablesEndpointMissing,
    #[error("SKIPPR_OFFSET_STORE=cloud-tables requires GuestCredentialBroker (CLOUD_SYSTEM_BROKER_CONFIG)")]
    CloudTablesAuthMissing,
    #[error("WAL_STORAGE=clustered requires SKIPPR_CLUSTER_ID")]
    ClusterIdMissing,
    #[error("WAL_STORAGE=clustered requires SKIPPR_CLUSTER_GOSSIP_KEY")]
    GossipKeyMissing,
    #[error("WAL_STORAGE=clustered requires SKIPPR_CLUSTER_TLS_CERT, SKIPPR_CLUSTER_TLS_KEY, and SKIPPR_CLUSTER_TLS_CA")]
    ClusterTlsMissing,
    #[error(
        "Iceberg catalog.table '{catalog_table}' must not be SKIPPR_OFFSET_DYNAMODB_TABLE '{offset_table}'; create a separate catalog table"
    )]
    SkipprCatalogReusesOffsetTable {
        catalog_table: String,
        offset_table: String,
    },
    #[error(
        "WAL_STORAGE=clustered cannot be used with SKIPPR_OFFSET_STORE={0}; clustered mode uses DynamoDB or Cloud tables offsets"
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
}
