use std::sync::atomic::{AtomicU64, Ordering};

use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

static ICEBERG_CAS_CONFLICTS: AtomicU64 = AtomicU64::new(0);

pub fn record_cas_conflict() {
    ICEBERG_CAS_CONFLICTS.fetch_add(1, Ordering::Relaxed);
}

pub fn take_cas_conflicts() -> u64 {
    ICEBERG_CAS_CONFLICTS.swap(0, Ordering::Relaxed)
}

#[derive(Debug, Deserialize, Serialize, Clone)]
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
    },
    Skippr {
        table: String,
        warehouse: String,
        #[serde(default)]
        region: Option<String>,
    },
    Rest {
        uri: String,
        warehouse: String,
    },
    Unity {
        uri: String,
        warehouse: String,
        #[serde(default)]
        token: Option<String>,
    },
    Polaris {
        uri: String,
        warehouse: String,
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        client_secret: Option<String>,
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
}
