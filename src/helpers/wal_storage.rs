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
}

impl WalStorage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disk => "disk",
            Self::S3 => "s3",
        }
    }

    pub fn uses_local_disk_segments(self) -> bool {
        match self {
            Self::Disk => true,
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

/// SkipprStore kind. Live engine is always fjall; leftover YAML `sled` maps here.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, Deserialize, Serialize)]
pub enum SkipprStoreKind {
    #[default]
    #[serde(rename = "fjall", alias = "sled")]
    #[value(name = "fjall", alias = "sled")]
    Fjall,
}

/// Durable Skippr KV store. Lock: `docs/docs/maintainers/skippr-store-kv.md`.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkipprStore {
    #[serde(rename = "type")]
    pub kind: SkipprStoreKind,
    /// Leftover YAML; ignored. Not a remote table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl SkipprStoreKind {
    pub fn as_str(self) -> &'static str {
        "fjall"
    }

    pub fn default_for_wal(_storage: WalStorage) -> Self {
        Self::Fjall
    }
}

impl FromStr for SkipprStoreKind {
    type Err = ConfigError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "sled" | "fjall" => Ok(Self::Fjall),
            other => Err(ConfigError::InvalidSkipprStore(other.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid WAL_STORAGE value '{0}'; expected disk or s3")]
    InvalidWalStorage(String),
    #[error("invalid skipprd_el_storage_mode value '{0}'; expected local or s3")]
    InvalidElStorageMode(String),
    #[error("invalid skippr.store.type value '{0}'; dynamodb is not Skippr Store (local fjall on DATA_DIR)")]
    InvalidSkipprStore(String),
    #[error("Iceberg sink config is invalid: {0}")]
    IcebergConfigInvalid(String),
    #[error("pipeline '{0}' not found")]
    PipelineNotFound(String),
    #[error("{0}")]
    InvalidIdentity(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_wal_storage_value() {
        assert_eq!("disk".parse::<WalStorage>().unwrap(), WalStorage::Disk);
        assert_eq!("S3".parse::<WalStorage>().unwrap(), WalStorage::S3);
        assert!("clustered".parse::<WalStorage>().is_err());
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
            SkipprStoreKind::Fjall
        );
        assert_eq!(
            SkipprStoreKind::default_for_wal(WalStorage::S3),
            SkipprStoreKind::Fjall
        );
    }

    #[test]
    fn cloud_tables_store_aliases_are_invalid() {
        for raw in ["cloud-tables", "cloud_tables", "tables", "CLOUD_TABLES"] {
            match raw.parse::<SkipprStoreKind>() {
                Err(ConfigError::InvalidSkipprStore(value)) => {
                    assert_eq!(value, raw.trim().to_ascii_lowercase());
                }
                other => panic!("{raw} should be invalid, got {other:?}"),
            }
        }
        assert!("dynamodb".parse::<SkipprStoreKind>().is_err());
        assert_eq!(
            "sled".parse::<SkipprStoreKind>().unwrap(),
            SkipprStoreKind::Fjall
        );
        assert_eq!(
            "fjall".parse::<SkipprStoreKind>().unwrap(),
            SkipprStoreKind::Fjall
        );
    }

    #[test]
    fn skippr_store_yaml_sled_maps_to_fjall() {
        let store: SkipprStore = serde_yaml::from_str("type: sled\n").unwrap();
        assert_eq!(store.kind, SkipprStoreKind::Fjall);
        assert!(serde_yaml::from_str::<SkipprStore>("type: dynamodb\nname: offsets\n").is_err());
    }

    #[test]
    fn skippr_store_yaml_rejects_removed_cloud_tables() {
        for raw in [
            "type: cloud-tables\nname: t\n",
            "type: cloud_tables\nname: t\n",
            "type: tables\nname: t\n",
        ] {
            assert!(
                serde_yaml::from_str::<SkipprStore>(raw).is_err(),
                "{raw} must not deserialize as SkipprStore"
            );
        }
    }

    #[test]
    fn skippr_store_kv_lock_is_the_only_store_authority() {
        let lock = include_str!("../../docs/docs/maintainers/skippr-store-kv.md");
        assert!(lock.contains("This file is the authority for Skippr Store"));
        assert!(
            lock.contains("No sharding."),
            "lock must forbid sharding rather than encode a zero shard id"
        );
        assert!(
            !lock.contains("always `0`"),
            "lock must not keep a dormant ShardId"
        );
        assert!(
            lock.contains("not a Store trait implementation"),
            "sled is a migrate reader, not a third trait impl"
        );
        assert!(
            lock.contains("skippr.store.type=sled after cutover maps to fjall"),
            "leftover YAML sled is not a live engine choice"
        );
        assert!(
            lock.contains("checksums differ, fail closed"),
            "local vs S3 metadata.json conflict must be fail-closed"
        );
        assert!(lock
            .contains("Ephemeral hosts without a durable DATA_DIR are unsupported this milestone"));
        assert!(lock.contains("MUST NOT select an Iceberg catalog backend via skippr.store.type"));
        assert!(
            lock.contains("Writer OCC is CatalogFence on trait commit"),
            "fence is CatalogFence, not a leases family"
        );
        assert!(
            !lock.to_ascii_lowercase().contains("future clustered query"),
            "do not name extra readers as clustered query"
        );

        let changelog = include_str!("../../CHANGELOG.md");
        assert!(
            !changelog.contains("Skippr Store is sled (local) or DynamoDB (remote)"),
            "CHANGELOG must not lock DynamoDB as Skippr Store"
        );
        assert!(
            !changelog.contains("Catalog pointers MAY share that table"),
            "CHANGELOG must not keep lake catalog as Skippr Store"
        );

        let readme = include_str!("../../README.md");
        assert!(
            !readme.contains("`clustered`"),
            "README must not list clustered WAL as a live option"
        );
        assert!(
            !readme.contains("writer lease unavailable"),
            "README must not document writer leases"
        );
        assert!(
            !readme.contains("SKIPPR_STORE_TYPE=dynamodb"),
            "README must not teach DynamoDB Skippr Store"
        );
        assert!(
            !readme.contains("type: dynamodb"),
            "README must not teach skippr.store.type dynamodb"
        );
        assert!(
            !readme.contains("DynamoDbStore"),
            "README must not teach Python DynamoDbStore"
        );
        assert!(
            !readme.contains("offsets & leases"),
            "README must not describe Skippr Store as leases"
        );
        assert!(
            !readme.contains("clustered quorum"),
            "README must not list clustered WAL quorum"
        );

        let cutover = include_str!("../../docs/docs/maintainers/warehouse-sinks-cutover.md");
        assert!(
            cutover.contains("skippr-store-kv.md"),
            "warehouse cutover must defer Skippr Store to skippr-store-kv.md"
        );
        assert!(
            !cutover.contains("from_store_type"),
            "lake catalog must not be selected via skippr.store.type"
        );
        assert!(
            !cutover.contains("MAY be the SkipprStore table"),
            "lake catalog_table is not Skippr Store"
        );

        let lake = include_str!("../../docs/docs/maintainers/skipprlake-dbt-rest.md");
        assert!(
            !lake.contains("offset-store-dynamodb"),
            "lake e2e must not require the Skippr Store DynamoDB feature"
        );

        let runtime_plugins = include_str!("../../docs/docs/maintainers/runtime-plugins.md");
        assert!(
            runtime_plugins.contains("skippr-store-kv.md"),
            "runtime-plugins must defer Skippr Store to the lock"
        );
        assert!(
            !runtime_plugins.contains("backed by `sled`"),
            "runtime-plugins must not name sled as the live offsets engine"
        );

        let plugin_contract =
            include_str!("../../docs/docs/maintainers/runtime-plugin-contract.md");
        assert!(
            plugin_contract.contains("skippr-store-kv.md"),
            "runtime-plugin-contract must defer Skippr Store to the lock"
        );

        let transforms = include_str!("../../docs/docs/maintainers/stream-transforms.md");
        assert!(
            !transforms.contains("distributed key ownership"),
            "MV must not plan sharding"
        );

        let e2e_ymls = [
            include_str!("../../.github/actions/e2e/postgres_skipprlake_cdc/skippr.yml"),
            include_str!("../../.github/actions/e2e/s3_skipprlake_alter/skippr.yml"),
            include_str!("../../.github/actions/e2e/bike_hire_many/skippr.yml"),
            include_str!("../../.github/actions/e2e/s3_skipprlake_evolve/skippr.yml"),
        ];
        for yml in e2e_ymls {
            assert!(
                !yml.contains("type: dynamodb"),
                "e2e skippr.yml must not select Skippr Store or lake catalog via store.type dynamodb"
            );
        }
        assert!(
            !include_str!("../../tests/skipprlake_e2e/run.py").contains("type: dynamodb"),
            "skipprlake e2e must not write skippr.store.type dynamodb"
        );
        assert!(
            !include_str!("../../tests/perf/run.py").contains("type: dynamodb"),
            "perf harness must not write skippr.store.type dynamodb"
        );
        assert!(
            !include_str!("../../tests/perf_db/run.py").contains("type: dynamodb"),
            "perf-db harness must not write skippr.store.type dynamodb"
        );
        assert!(
            !include_str!("../../skippr.pyi").contains("class SledStore"),
            "Python stub must not export SledStore"
        );
        assert!(
            !include_str!("../../skippr.pyi").contains("class DynamoDbStore"),
            "Python stub must not export DynamoDbStore"
        );
        assert!(
            include_str!("../../skippr.pyi").contains("class FjallStore"),
            "Python stub must export FjallStore"
        );
        assert!(
            !include_str!("../../src/helpers/offsets.rs").contains("fn vacuum("),
            "sled vacuum is not Skippr Store"
        );
        assert!(
            !include_str!("../../src/sqlrt/query.rs").contains("metadata/metadata.json"),
            "SHOW PIPELINE must not advertise leftover metadata.json as authority"
        );
        assert!(
            !include_str!("../../plugins/data_source/kinesis/src/kinesis.rs")
                .contains("kinesis_checkpoints"),
            "Kinesis resume must not be plugin-local seq files"
        );
        assert!(
            include_str!("../../plugins/data_source/kinesis/src/kinesis.rs")
                .contains("store_checkpoint_payload"),
            "Kinesis resume must use host Skippr Store checkpoints"
        );
        assert!(
            !include_str!("../../src/failpoint.rs").contains("clustered failpoint"),
            "catalog failpoints must not keep clustered WAL wording"
        );
        assert!(
            !include_str!("../../src/failpoint.rs").contains("AfterReplicaPayloadAck"),
            "replica payload-ack failpoint is clustered WAL"
        );
        assert!(
            !include_str!("../../crates/skippr-iceberg-catalog/src/lib.rs")
                .contains("MAY share the SkipprStore table"),
            "lake catalog rustdoc must not share Skippr Store"
        );
        assert!(
            !include_str!("../../docs/docs/connectors/outputs/skipprlake.md")
                .contains("May be the SkipprStore table"),
            "public SkipprLake docs must not share Skippr Store"
        );
        assert!(
            !include_str!("../../skippr.pyi").contains("MAY share the SkipprStore table"),
            "Python stub must not share Skippr Store with the lake catalog"
        );
        assert!(
            !include_str!("../../updates.md").contains("\"metadata_location\""),
            "updates.md must not advertise leftover metadata.json as SHOW PIPELINE authority"
        );
        assert!(
            !include_str!("../../todo.md").contains("\"metadata_location\""),
            "todo.md must not advertise leftover metadata.json as SHOW PIPELINE authority"
        );
        assert!(
            !include_str!("../../todo.md").contains("sled-based"),
            "todo.md must not name sled as the live offsets engine"
        );
        assert!(
            !include_str!("../../src/sqlrt/query.rs").contains("offset database"),
            "RESET PIPELINE must not say offset database"
        );
        assert!(
            !include_str!("../../src/sqlrt/docs.rs")
                .contains("namespaces (name, enabled, fields), offsets, and schema"),
            "SHOW PIPELINE docs must not claim a live offsets dump"
        );
        assert!(
            !include_str!("../../.cursor/skills/skippr-perf-db/SKILL.md")
                .contains("DynamoDB Local Skippr Store"),
            "perf-db skill must not use DynamoDB as Skippr Store"
        );
        assert!(
            !include_str!("../../src/helpers/configuration.rs").contains("in Sled"),
            "stream aggregates live in Skippr Store, not Sled"
        );
        assert!(
            !include_str!("../../src/helpers/offsets.rs").contains("sled::IVec"),
            "offset API must not be typed as sled IVec"
        );
        assert!(
            !include_str!("../../docs/docs/configuration/overview.md")
                .contains("--store-type <fjall|sled>"),
            "public docs must not list sled as a live --store-type peer"
        );
    }
}
