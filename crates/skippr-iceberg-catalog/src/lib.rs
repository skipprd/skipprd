use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use iceberg::io::{FileIO, FileIOBuilder};
use iceberg::{Error, ErrorKind, Result as IcebergResult};

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

/// Warehouse identity: trim whitespace then trailing slashes.
pub fn warehouse_key(warehouse: &str) -> String {
    warehouse.trim().trim_end_matches('/').to_string()
}

pub fn require_file_warehouse(warehouse: &str) -> Result<String, String> {
    if warehouse.trim().is_empty() {
        return Err("warehouse is required".into());
    }
    let key = warehouse_key(warehouse);
    if !key.starts_with("file://") {
        return Err("warehouse must be file://".into());
    }
    Ok(key)
}

pub fn validate_file_warehouse(warehouse: &str, table_namespace: &str) -> Result<(), String> {
    require_file_warehouse(warehouse)?;
    if table_namespace.trim().is_empty() {
        return Err("table_namespace is required".into());
    }
    Ok(())
}

/// AthenaIceberg / Glue parquet location. Local FS is not a nameable state.
/// YAML key is `object_store`. `s3` is the AWS default chain. `r2` is explicit
/// path-style credentials.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, SkipprConfig, Default)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum S3CompatibleObjectStore {
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

/// SkipprLake parquet location: local FS, AWS S3, or R2.
/// YAML key is `object_store` (not `file_io`, not Apache Iceberg `FileIO`).
/// AthenaIceberg MUST use [`S3CompatibleObjectStore`] so `File` cannot be named.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, SkipprConfig, Default)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WarehouseObjectStore {
    File,
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

fn default_lake_namespace() -> String {
    "default".to_string()
}

/// Config for the `SkipprLake` data sink and schema sink.
/// Query and serve wrap this in `SkipprLakeOpen` (plus `SkipprCatalogBackend`)
/// inside `IcebergCatalogSpec`, not as a standalone query backend.
/// The Skippr catalog is the only catalog: pointers live in `catalog_table`
/// (DynamoDB, or Cloud Tables when SkipprStore selects it). Catalog rows MAY
/// share the SkipprStore table; PK/SK prefixes do not collide with offsets.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, SkipprConfig)]
#[serde(deny_unknown_fields)]
pub struct SkipprLakeConfig {
    /// Iceberg warehouse root: `s3://bucket/path` or `file:///abs/path`.
    pub warehouse: String,
    /// Catalog pointer table. MAY be the SkipprStore table.
    pub catalog_table: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub object_store: WarehouseObjectStore,
    /// Iceberg namespace for sink-managed tables. Unique per `(catalog_table, table_namespace)`.
    #[serde(default = "default_lake_namespace")]
    pub table_namespace: String,
}

impl SkipprLakeConfig {
    pub const PLUGIN_NAME: &'static str = "SkipprLake";

    /// `{warehouse}/{table_namespace}`: the root of every table this sink creates.
    pub fn location_root(&self) -> String {
        format!(
            "{}/{}",
            self.warehouse.trim_end_matches('/'),
            self.table_namespace
        )
    }

    pub fn is_plugin_name(name: &str) -> bool {
        name.eq_ignore_ascii_case(Self::PLUGIN_NAME)
    }

    /// Warehouse URI must match `object_store`. Default store is S3; `file://`
    /// lakes MUST set `object_store: { type: file }`.
    pub fn validate(&self) -> Result<(), String> {
        if self.warehouse.trim().is_empty() {
            return Err("warehouse is required".into());
        }
        if self.catalog_table.trim().is_empty() {
            return Err("catalog_table is required".into());
        }
        if self.table_namespace.trim().is_empty() {
            return Err("table_namespace is required".into());
        }
        self.object_store.validate_warehouse(&self.warehouse)?;
        match &self.object_store {
            WarehouseObjectStore::R2 { .. } => {
                s3_object_store_props(&self.object_store.s3_compatible()?)?;
            }
            WarehouseObjectStore::File | WarehouseObjectStore::S3 => {}
        }
        Ok(())
    }
}

/// Which Skippr catalog pointer backend the lake uses. Derived from SkipprStore
/// type — one function for the plugin process and the query host.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipprCatalogBackend {
    DynamoDb,
    CloudTables,
}

impl SkipprCatalogBackend {
    pub fn from_store_type(raw: &str) -> Self {
        if store_is_cloud_tables(raw) {
            Self::CloudTables
        } else {
            Self::DynamoDb
        }
    }

    /// Deprecated: `SKIPPR_OFFSET_STORE`. Use [`Self::from_store_type`].
    pub fn from_offset_store(raw: &str) -> Self {
        Self::from_store_type(raw)
    }
}

/// True when SkipprStore type selects Cloud Tables (same aliases as skipprd).
pub fn store_is_cloud_tables(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "cloud-tables" | "cloud_tables" | "tables"
    )
}

/// Deprecated: `SKIPPR_OFFSET_STORE`. Use [`store_is_cloud_tables`].
pub fn offset_store_is_cloud_tables(raw: &str) -> bool {
    store_is_cloud_tables(raw)
}

/// `SKIPPR_STORE_TYPE`, falling back to deprecated `SKIPPR_OFFSET_STORE`.
pub fn store_type_from_env() -> String {
    let fresh = std::env::var("SKIPPR_STORE_TYPE").unwrap_or_default();
    if !fresh.trim().is_empty() {
        return fresh;
    }
    let deprecated = std::env::var("SKIPPR_OFFSET_STORE").unwrap_or_default();
    deprecated
}

/// The `s3a://` scheme iceberg-rust/OpenDAL expects for an `s3://` location.
pub fn to_iceberg_s3_uri(uri: &str) -> String {
    if let Some(rest) = uri.strip_prefix("s3://") {
        format!("s3a://{rest}")
    } else {
        uri.to_string()
    }
}

pub fn warehouse_hash(warehouse: &str) -> String {
    let normalized = warehouse_key(warehouse);
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    hex_encode(hasher.finalize().as_slice())
}

pub fn require_s3_warehouse(warehouse: &str) -> Result<String, String> {
    if warehouse.trim().is_empty() {
        return Err("warehouse is required".into());
    }
    let key = warehouse_key(warehouse);
    if !(key.starts_with("s3://") || key.starts_with("s3a://")) {
        return Err("warehouse must be s3://".into());
    }
    Ok(key)
}

/// `{warehouse}/{namespace}/{table}` — writer and REST/catalog create share this.
pub fn iceberg_table_location(warehouse: &str, namespace: &str, table: &str) -> String {
    format!(
        "{}/{}/{}",
        warehouse.trim().trim_end_matches('/'),
        namespace.trim().trim_matches('/'),
        table.trim().trim_matches('/')
    )
}

impl S3CompatibleObjectStore {
    pub fn physical_key(&self) -> String {
        match self {
            Self::S3 => "s3".into(),
            Self::R2 { endpoint, .. } => format!("r2:{endpoint}"),
        }
    }

    pub fn validate_warehouse(&self, warehouse: &str) -> Result<(), String> {
        require_s3_warehouse(warehouse).map(|_| ())
    }
}

impl WarehouseObjectStore {
    pub fn physical_key(&self) -> String {
        match self {
            Self::File => "file".into(),
            Self::S3 => "s3".into(),
            Self::R2 { endpoint, .. } => format!("r2:{endpoint}"),
        }
    }

    pub fn validate_warehouse(&self, warehouse: &str) -> Result<(), String> {
        match self {
            Self::File => require_file_warehouse(warehouse).map(|_| ()),
            Self::S3 | Self::R2 { .. } => require_s3_warehouse(warehouse).map(|_| ()),
        }
    }

    pub fn s3_compatible(&self) -> Result<S3CompatibleObjectStore, String> {
        match self {
            Self::File => Err("object_store type file is not S3-compatible".into()),
            Self::S3 => Ok(S3CompatibleObjectStore::S3),
            Self::R2 {
                endpoint,
                region,
                access_key_id,
                secret_access_key,
                path_style,
            } => Ok(S3CompatibleObjectStore::R2 {
                endpoint: endpoint.clone(),
                region: region.clone(),
                access_key_id: access_key_id.clone(),
                secret_access_key: secret_access_key.clone(),
                path_style: *path_style,
            }),
        }
    }
}

/// OpenDal/Iceberg S3 properties from typed S3-compatible `object_store`.
/// `S3` uses the AWS default chain (empty props). `R2` requires endpoint and keys.
pub fn s3_object_store_props(
    object_store: &S3CompatibleObjectStore,
) -> Result<HashMap<String, String>, String> {
    match object_store {
        S3CompatibleObjectStore::S3 => Ok(HashMap::new()),
        S3CompatibleObjectStore::R2 {
            endpoint,
            region,
            access_key_id,
            secret_access_key,
            path_style,
        } => r2_object_store_props(
            endpoint,
            region.as_deref(),
            access_key_id,
            secret_access_key,
            *path_style,
        ),
    }
}

fn r2_object_store_props(
    endpoint: &str,
    region: Option<&str>,
    access_key_id: &str,
    secret_access_key: &str,
    path_style: bool,
) -> Result<HashMap<String, String>, String> {
    let endpoint = endpoint.trim();
    let access_key_id = access_key_id.trim();
    let secret_access_key = secret_access_key.trim();
    if endpoint.is_empty() || access_key_id.is_empty() || secret_access_key.is_empty() {
        return Err(
            "object_store type r2 requires endpoint, access_key_id, and secret_access_key".into(),
        );
    }
    let mut props = HashMap::new();
    props.insert("s3.endpoint".into(), endpoint.to_string());
    props.insert(
        "s3.path-style-access".into(),
        if path_style { "true" } else { "false" }.into(),
    );
    props.insert("s3.access-key-id".into(), access_key_id.to_string());
    props.insert("s3.secret-access-key".into(), secret_access_key.to_string());
    props.insert("s3.disable-config-load".into(), "true".into());
    props.insert("s3.disable-ec2-metadata".into(), "true".into());
    if let Some(region) = region.map(str::trim).filter(|value| !value.is_empty()) {
        props.insert("s3.region".into(), region.to_string());
    }
    Ok(props)
}

pub fn iceberg_file_io_for(
    warehouse: &str,
    object_store: &WarehouseObjectStore,
) -> IcebergResult<FileIO> {
    object_store
        .validate_warehouse(warehouse)
        .map_err(|err| Error::new(ErrorKind::DataInvalid, err))?;
    match object_store {
        WarehouseObjectStore::File => Ok(FileIO::new_with_fs()),
        WarehouseObjectStore::S3 | WarehouseObjectStore::R2 { .. } => {
            let compatible = object_store
                .s3_compatible()
                .map_err(|err| Error::new(ErrorKind::DataInvalid, err))?;
            iceberg_file_io_s3(warehouse, &compatible)
        }
    }
}

pub fn iceberg_file_io_s3(
    warehouse: &str,
    object_store: &S3CompatibleObjectStore,
) -> IcebergResult<FileIO> {
    object_store
        .validate_warehouse(warehouse)
        .map_err(|err| Error::new(ErrorKind::DataInvalid, err))?;
    let props = s3_object_store_props(object_store)
        .map_err(|err| Error::new(ErrorKind::DataInvalid, err))?;
    Ok(FileIOBuilder::new(Arc::new(
        iceberg_storage_opendal::OpenDalStorageFactory::S3 {
            configured_scheme: "s3".to_string(),
            customized_credential_load: None,
        },
    ))
    .with_props(props)
    .build())
}

pub fn iceberg_file_io_for_warehouse(config: &SkipprLakeConfig) -> IcebergResult<FileIO> {
    config
        .validate()
        .map_err(|err| Error::new(ErrorKind::DataInvalid, err))?;
    iceberg_file_io_for(&config.warehouse, &config.object_store)
}

fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn duckdb_s3_endpoint(endpoint: &str) -> String {
    endpoint
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

impl WarehouseObjectStore {
    /// DuckDB `httpfs`/`iceberg` CREATE SECRET. R2 uses path-style S3.
    pub fn duckdb_create_secret_sql(&self) -> Result<String, String> {
        match self {
            Self::File => Err("object_store type file has no duckdb secret".into()),
            Self::S3 => {
                Ok("CREATE OR REPLACE SECRET (TYPE S3, PROVIDER CREDENTIAL_CHAIN);".to_string())
            }
            Self::R2 {
                endpoint,
                region,
                access_key_id,
                secret_access_key,
                path_style,
            } => {
                let endpoint = duckdb_s3_endpoint(endpoint);
                let access_key_id = access_key_id.trim();
                let secret_access_key = secret_access_key.trim();
                if endpoint.is_empty() || access_key_id.is_empty() || secret_access_key.is_empty() {
                    return Err(
                        "object_store type r2 requires endpoint, access_key_id, and secret_access_key"
                            .into(),
                    );
                }
                let region = region
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or("auto");
                let url_style = if *path_style { "path" } else { "vhost" };
                Ok(format!(
                    "CREATE OR REPLACE SECRET (TYPE S3, KEY_ID {}, SECRET {}, REGION {}, ENDPOINT {}, URL_STYLE '{}');",
                    sql_string_literal(access_key_id),
                    sql_string_literal(secret_access_key),
                    sql_string_literal(region),
                    sql_string_literal(&endpoint),
                    url_style,
                ))
            }
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
        assert_eq!(
            warehouse_hash("  s3://bucket/wh/  "),
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

    fn lake(value: serde_json::Value) -> Result<SkipprLakeConfig, serde_json::Error> {
        serde_json::from_value(value)
    }

    #[test]
    fn skipprlake_config_is_flat_with_default_namespace() {
        let cfg = lake(serde_json::json!({
            "catalog_table": "skippr-hla",
            "warehouse": "s3://bucket/warehouse/"
        }))
        .unwrap();
        assert_eq!(cfg.catalog_table, "skippr-hla");
        assert_eq!(cfg.table_namespace, "default");
        assert_eq!(cfg.location_root(), "s3://bucket/warehouse/default");
        cfg.validate().unwrap();
    }

    #[test]
    fn cloud_tables_store_aliases_match_skipprd() {
        assert!(store_is_cloud_tables("cloud-tables"));
        assert!(store_is_cloud_tables("CLOUD_TABLES"));
        assert!(store_is_cloud_tables(" tables "));
        assert!(!store_is_cloud_tables("dynamodb"));
        assert!(!store_is_cloud_tables(""));
    }

    #[test]
    fn catalog_backend_follows_store_type_aliases() {
        assert_eq!(
            SkipprCatalogBackend::from_store_type("cloud-tables"),
            SkipprCatalogBackend::CloudTables
        );
        assert_eq!(
            SkipprCatalogBackend::from_store_type("CLOUD_TABLES"),
            SkipprCatalogBackend::CloudTables
        );
        assert_eq!(
            SkipprCatalogBackend::from_store_type("dynamodb"),
            SkipprCatalogBackend::DynamoDb
        );
        assert_eq!(
            SkipprCatalogBackend::from_store_type(""),
            SkipprCatalogBackend::DynamoDb
        );
    }

    #[test]
    fn iceberg_file_io_for_file_store_requires_file_uri() {
        super::iceberg_file_io_for("file:///tmp/lake", &WarehouseObjectStore::File).unwrap();
        let err = super::iceberg_file_io_for("s3://bucket/wh", &WarehouseObjectStore::File)
            .unwrap_err()
            .to_string();
        assert!(err.contains("file://"), "{err}");
        let err = super::iceberg_file_io_for("file:///tmp/lake", &WarehouseObjectStore::S3)
            .unwrap_err()
            .to_string();
        assert!(err.contains("s3://"), "{err}");
    }

    #[test]
    fn skipprlake_file_warehouse_requires_file_object_store() {
        let omitted = lake(serde_json::json!({
            "catalog_table": "skippr-hla",
            "warehouse": "file:///tmp/warehouse/"
        }))
        .unwrap();
        assert_eq!(omitted.object_store, WarehouseObjectStore::S3);
        let err = omitted.validate().unwrap_err();
        assert!(err.contains("s3://"), "{err}");
        let cfg = lake(serde_json::json!({
            "catalog_table": "skippr-hla",
            "warehouse": "file:///tmp/warehouse/",
            "object_store": { "type": "file" }
        }))
        .unwrap();
        cfg.validate().unwrap();
        super::iceberg_file_io_for_warehouse(&cfg).unwrap();
    }

    #[test]
    fn iceberg_table_location_includes_namespace() {
        assert_eq!(
            super::iceberg_table_location("file:///lake/", "analytics", "fct_orders"),
            "file:///lake/analytics/fct_orders"
        );
        assert_eq!(
            super::iceberg_table_location("s3://bucket/wh", "bronze", "orders"),
            "s3://bucket/wh/bronze/orders"
        );
    }

    #[test]
    fn s3_compatible_store_cannot_name_file() {
        let err = serde_json::from_value::<S3CompatibleObjectStore>(serde_json::json!({
            "type": "file"
        }))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("unknown variant") || err.contains("file"),
            "{err}"
        );
    }

    #[test]
    fn warehouse_key_trims_whitespace_and_trailing_slash() {
        assert_eq!(
            super::warehouse_key("  file:///tmp/lake/  "),
            "file:///tmp/lake"
        );
        assert_eq!(super::warehouse_key("file:///tmp/lake"), "file:///tmp/lake");
    }

    #[test]
    fn validate_file_warehouse_requires_file_uri() {
        super::validate_file_warehouse("file:///tmp/lake", "bronze").unwrap();
        assert!(super::validate_file_warehouse("s3://bucket/wh", "bronze")
            .unwrap_err()
            .contains("file://"));
        assert!(super::validate_file_warehouse("/tmp/lake", "bronze")
            .unwrap_err()
            .contains("file://"));
        assert!(super::validate_file_warehouse("  ", "bronze")
            .unwrap_err()
            .contains("warehouse is required"));
        assert!(super::validate_file_warehouse("file:///tmp/lake", "  ")
            .unwrap_err()
            .contains("table_namespace is required"));
    }

    #[test]
    fn s3_object_store_default_is_empty_aws_chain() {
        let props = super::s3_object_store_props(&S3CompatibleObjectStore::S3).unwrap();
        assert!(props.is_empty());
        let cfg = lake(serde_json::json!({
            "catalog_table": "skippr-hla",
            "warehouse": "s3://bucket/wh"
        }))
        .unwrap();
        assert_eq!(cfg.object_store, WarehouseObjectStore::S3);
        assert!(
            super::s3_object_store_props(&cfg.object_store.s3_compatible().unwrap())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn r2_object_store_maps_to_duckdb_path_style_secret() {
        let sql = WarehouseObjectStore::R2 {
            endpoint: "https://account.r2.cloudflarestorage.com/".into(),
            region: Some("auto".into()),
            access_key_id: "ak".into(),
            secret_access_key: "sk'quote".into(),
            path_style: true,
        }
        .duckdb_create_secret_sql()
        .unwrap();
        assert!(sql.contains("ENDPOINT 'account.r2.cloudflarestorage.com'"));
        assert!(sql.contains("URL_STYLE 'path'"));
        assert!(sql.contains("SECRET 'sk''quote'"));
        assert!(sql.contains("REGION 'auto'"));
    }

    #[test]
    fn s3_object_store_uses_credential_chain_secret() {
        let sql = WarehouseObjectStore::S3.duckdb_create_secret_sql().unwrap();
        assert!(sql.contains("PROVIDER CREDENTIAL_CHAIN"));
    }

    #[test]
    fn removed_and_foreign_keys_are_unknown_fields() {
        for (key, value) in [
            ("catalog", serde_json::json!({ "type": "skippr" })),
            ("table_prefix", serde_json::json!("p")),
            ("table_location_prefix", serde_json::json!("s3://b/x")),
            ("properties", serde_json::json!({})),
            ("format", serde_json::json!("parquet")),
            ("query_engine", serde_json::json!({ "type": "skippr" })),
            ("file_io", serde_json::json!({ "type": "s3" })),
        ] {
            let mut doc = serde_json::json!({
                "catalog_table": "skippr-hla",
                "warehouse": "s3://bucket/wh",
            });
            doc[key] = value;
            let err = lake(doc).unwrap_err().to_string();
            assert!(
                err.contains("unknown field"),
                "{key} must be rejected: {err}"
            );
        }
    }

    #[test]
    fn r2_object_store_props_come_from_catalog_not_env() {
        let object_store = WarehouseObjectStore::R2 {
            endpoint: "https://r2.example".into(),
            region: Some("auto".into()),
            access_key_id: "objects-key".into(),
            secret_access_key: "objects-secret".into(),
            path_style: true,
        };
        let props = super::s3_object_store_props(&object_store.s3_compatible().unwrap()).unwrap();
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
        let cfg = lake(serde_json::json!({
            "catalog_table": "skippr-hla",
            "warehouse": "s3://bucket/wh",
            "object_store": {
                "type": "r2",
                "endpoint": "${OBJECTS_S3_ENDPOINT}",
                "region": "auto",
                "access_key_id": "${OBJECTS_ACCESS_KEY_ID}",
                "secret_access_key": "${OBJECTS_SECRET_ACCESS_KEY}",
                "path_style": true
            }
        }))
        .unwrap();
        match &cfg.object_store {
            WarehouseObjectStore::R2 {
                endpoint,
                access_key_id,
                secret_access_key,
                ..
            } => {
                assert_eq!(endpoint, "${OBJECTS_S3_ENDPOINT}");
                assert_eq!(access_key_id, "${OBJECTS_ACCESS_KEY_ID}");
                assert_eq!(secret_access_key, "${OBJECTS_SECRET_ACCESS_KEY}");
            }
            WarehouseObjectStore::S3 | WarehouseObjectStore::File => {
                panic!("r2 fixture must not default to s3")
            }
        }
    }

    #[test]
    fn r2_object_store_fails_closed_without_keys() {
        let err = super::s3_object_store_props(&S3CompatibleObjectStore::R2 {
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
