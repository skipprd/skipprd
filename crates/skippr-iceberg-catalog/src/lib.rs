use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use skippr_plugin_macros::SkipprConfig;

static ICEBERG_CAS_CONFLICTS: AtomicU64 = AtomicU64::new(0);

pub fn record_cas_conflict() {
    ICEBERG_CAS_CONFLICTS.fetch_add(1, Ordering::Relaxed);
}

pub fn take_cas_conflicts() -> u64 {
    ICEBERG_CAS_CONFLICTS.swap(0, Ordering::Relaxed)
}

/// Object-store FileIO for Iceberg parquet. `s3` is the AWS default chain.
/// `r2` is explicit S3-compatible credentials (Cloudflare R2 / path-style).
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, SkipprConfig, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IcebergFileIo {
    #[default]
    S3,
    R2 {
        endpoint: String,
        #[serde(default)]
        region: Option<String>,
        access_key_id: String,
        #[skippr(secret)]
        secret_access_key: String,
        #[serde(default = "default_r2_path_style")]
        path_style: bool,
    },
}

fn default_r2_path_style() -> bool {
    true
}

#[derive(Debug, Deserialize, Serialize, Clone, SkipprConfig)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IcebergCatalogConfig {
    Glue {
        warehouse: String,
        #[serde(default)]
        database: Option<String>,
        #[serde(default)]
        catalog_id: Option<String>,
        #[serde(default)]
        region: Option<String>,
        #[serde(default)]
        file_io: IcebergFileIo,
    },
    Skippr {
        table: String,
        warehouse: String,
        #[serde(default)]
        region: Option<String>,
        #[serde(default)]
        file_io: IcebergFileIo,
    },
    Rest {
        uri: String,
        warehouse: String,
        #[serde(default)]
        file_io: IcebergFileIo,
    },
    Unity {
        uri: String,
        warehouse: String,
        #[serde(default)]
        #[skippr(secret)]
        token: Option<String>,
        #[serde(default)]
        file_io: IcebergFileIo,
    },
    Polaris {
        uri: String,
        warehouse: String,
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        #[skippr(secret)]
        client_secret: Option<String>,
        #[serde(default)]
        file_io: IcebergFileIo,
    },
}

impl IcebergCatalogConfig {
    pub fn adapter_name(&self) -> &'static str {
        match self {
            Self::Glue { .. } => "glue",
            Self::Skippr { .. } => "skippr",
            Self::Rest { .. } => "rest",
            Self::Unity { .. } => "unity",
            Self::Polaris { .. } => "polaris",
        }
    }

    pub fn warehouse(&self) -> &str {
        match self {
            Self::Glue { warehouse, .. }
            | Self::Skippr { warehouse, .. }
            | Self::Rest { warehouse, .. }
            | Self::Unity { warehouse, .. }
            | Self::Polaris { warehouse, .. } => warehouse,
        }
    }

    pub fn skippr_table(&self) -> Option<&str> {
        match self {
            Self::Skippr { table, .. } => Some(table.as_str()),
            Self::Glue { .. } | Self::Rest { .. } | Self::Unity { .. } | Self::Polaris { .. } => {
                None
            }
        }
    }

    pub fn file_io(&self) -> &IcebergFileIo {
        match self {
            Self::Glue { file_io, .. }
            | Self::Skippr { file_io, .. }
            | Self::Rest { file_io, .. }
            | Self::Unity { file_io, .. }
            | Self::Polaris { file_io, .. } => file_io,
        }
    }
}

/// True when `SKIPPR_OFFSET_STORE` selects Cloud Tables (same aliases as skipprd).
pub fn offset_store_is_cloud_tables(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "cloud-tables" | "cloud_tables" | "tables"
    )
}

/// Product: Iceberg `catalog.table` is a customer-created catalog table, not the offset/lease table.
pub fn skippr_catalog_reuses_offset_table(catalog_table: &str, offset_table: &str) -> bool {
    let catalog = catalog_table.trim();
    let offset = offset_table.trim();
    !catalog.is_empty() && !offset.is_empty() && catalog == offset
}

pub fn warehouse_hash(warehouse: &str) -> String {
    let normalized = warehouse.trim_end_matches('/');
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    hex_encode(hasher.finalize().as_slice())
}

/// Iceberg FileIO S3 properties from typed `catalog.file_io`.
/// `S3` uses the AWS default chain (empty props). `R2` requires endpoint and keys.
pub fn s3_file_io_props(file_io: &IcebergFileIo) -> Result<HashMap<String, String>, String> {
    match file_io {
        IcebergFileIo::S3 => Ok(HashMap::new()),
        IcebergFileIo::R2 {
            endpoint,
            region,
            access_key_id,
            secret_access_key,
            path_style,
        } => {
            let endpoint = endpoint.trim();
            let access_key_id = access_key_id.trim();
            let secret_access_key = secret_access_key.trim();
            if endpoint.is_empty() || access_key_id.is_empty() || secret_access_key.is_empty() {
                return Err(
                    "catalog.file_io type r2 requires endpoint, access_key_id, and secret_access_key"
                        .into(),
                );
            }
            let mut props = HashMap::new();
            props.insert("s3.endpoint".into(), endpoint.to_string());
            props.insert(
                "s3.path-style-access".into(),
                if *path_style { "true" } else { "false" }.into(),
            );
            props.insert("s3.access-key-id".into(), access_key_id.to_string());
            props.insert("s3.secret-access-key".into(), secret_access_key.to_string());
            props.insert("s3.disable-config-load".into(), "true".into());
            props.insert("s3.disable-ec2-metadata".into(), "true".into());
            if let Some(region) = region
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                props.insert("s3.region".into(), region.to_string());
            }
            Ok(props)
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn encode_name(name: &str) -> String {
    format!("{}:{name}", name.len())
}

pub fn decode_name(encoded: &str) -> Result<String, String> {
    let (len, rest) = encoded
        .split_once(':')
        .ok_or_else(|| "missing length prefix".to_string())?;
    let len: usize = len.parse::<usize>().map_err(|err| err.to_string())?;
    if rest.len() != len {
        return Err("length prefix does not match name".into());
    }
    Ok(rest.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_encoding_round_trips_and_does_not_split_on_delimiters() {
        let original = "ns#with/slash";
        let encoded = encode_name(original);
        assert_eq!(encoded, "13:ns#with/slash");
        assert_eq!(decode_name(&encoded).unwrap(), original);
        assert_eq!(decode_name(&encode_name("foo:bar")).unwrap(), "foo:bar");
    }

    #[test]
    fn warehouse_hash_is_stable() {
        assert_eq!(
            warehouse_hash("s3://bucket/wh/"),
            warehouse_hash("s3://bucket/wh")
        );
    }

    #[test]
    fn cas_conflict_counter_drains() {
        let _ = take_cas_conflicts();
        record_cas_conflict();
        record_cas_conflict();
        assert_eq!(take_cas_conflicts(), 2);
        assert_eq!(take_cas_conflicts(), 0);
    }

    #[test]
    fn skippr_catalog_type_deserializes_from_yaml_name() {
        let cfg: IcebergCatalogConfig = serde_json::from_value(serde_json::json!({
            "type": "skippr",
            "table": "skippr-hla",
            "warehouse": "file:///tmp/warehouse"
        }))
        .unwrap();
        assert_eq!(cfg.adapter_name(), "skippr");
        assert_eq!(cfg.warehouse(), "file:///tmp/warehouse");
        assert_eq!(cfg.skippr_table(), Some("skippr-hla"));
        assert!(skippr_catalog_reuses_offset_table(
            "skippr-hla",
            "skippr-hla"
        ));
        assert!(!skippr_catalog_reuses_offset_table(
            "skippr-iceberg-catalog",
            "skippr-hla"
        ));
    }

    #[test]
    fn cloud_tables_offset_store_aliases_match_skipprd() {
        assert!(offset_store_is_cloud_tables("cloud-tables"));
        assert!(offset_store_is_cloud_tables("CLOUD_TABLES"));
        assert!(offset_store_is_cloud_tables(" tables "));
        assert!(!offset_store_is_cloud_tables("dynamodb"));
        assert!(!offset_store_is_cloud_tables(""));
    }

    #[test]
    fn s3_file_io_default_is_empty_aws_chain() {
        let props = super::s3_file_io_props(&IcebergFileIo::S3).unwrap();
        assert!(props.is_empty());
        let cfg: IcebergCatalogConfig = serde_json::from_value(serde_json::json!({
            "type": "skippr",
            "table": "skippr-hla",
            "warehouse": "s3://bucket/wh"
        }))
        .unwrap();
        assert_eq!(cfg.file_io(), &IcebergFileIo::S3);
        assert!(super::s3_file_io_props(cfg.file_io()).unwrap().is_empty());
    }

    #[test]
    fn r2_file_io_props_come_from_catalog_not_env() {
        let file_io = IcebergFileIo::R2 {
            endpoint: "https://r2.example".into(),
            region: Some("auto".into()),
            access_key_id: "objects-key".into(),
            secret_access_key: "objects-secret".into(),
            path_style: true,
        };
        let props = super::s3_file_io_props(&file_io).unwrap();
        assert_eq!(
            props.get("s3.endpoint").map(String::as_str),
            Some("https://r2.example")
        );
        assert_eq!(
            props.get("s3.access-key-id").map(String::as_str),
            Some("objects-key")
        );
        assert_eq!(
            props.get("s3.secret-access-key").map(String::as_str),
            Some("objects-secret")
        );
        assert_eq!(props.get("s3.region").map(String::as_str), Some("auto"));
        assert_eq!(
            props.get("s3.path-style-access").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            props.get("s3.disable-config-load").map(String::as_str),
            Some("true")
        );
        let cfg: IcebergCatalogConfig = serde_json::from_value(serde_json::json!({
            "type": "skippr",
            "table": "skippr-hla",
            "warehouse": "s3://bucket/wh",
            "file_io": {
                "type": "r2",
                "endpoint": "${OBJECTS_S3_ENDPOINT}",
                "region": "auto",
                "access_key_id": "${OBJECTS_ACCESS_KEY_ID}",
                "secret_access_key": "${OBJECTS_SECRET_ACCESS_KEY}",
                "path_style": true
            }
        }))
        .unwrap();
        match cfg.file_io() {
            IcebergFileIo::R2 {
                endpoint,
                access_key_id,
                secret_access_key,
                ..
            } => {
                assert_eq!(endpoint, "${OBJECTS_S3_ENDPOINT}");
                assert_eq!(access_key_id, "${OBJECTS_ACCESS_KEY_ID}");
                assert_eq!(secret_access_key, "${OBJECTS_SECRET_ACCESS_KEY}");
            }
            IcebergFileIo::S3 => panic!("r2 fixture must not default to s3"),
        }
    }

    #[test]
    fn r2_file_io_fails_closed_without_keys() {
        let err = super::s3_file_io_props(&IcebergFileIo::R2 {
            endpoint: "".into(),
            region: None,
            access_key_id: "k".into(),
            secret_access_key: "s".into(),
            path_style: true,
        })
        .unwrap_err();
        assert!(err.contains("type r2 requires"));
    }
}
