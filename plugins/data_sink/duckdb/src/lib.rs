use std::io;
use std::sync::Arc;

use iceberg::io::FileIO;
use iceberg::Catalog;
use serde_derive::{Deserialize, Serialize};
use skippr_iceberg_catalog_fs::FsCatalog;
pub use skippr_iceberg_writer::{IcebergWriter, IcebergWriterConfig};
use skippr_runtime_sdk::plugins::cdc::sink_capabilities;
use skippr_runtime_sdk::plugins::source_contract::SinkWritePolicySupport;
use skippr_runtime_sdk::SkipprConfig;

skippr_runtime_sdk::declare_sink_spec!(
    DuckdbSinkSpec,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::DUCKDB,
    skippr_runtime_sdk::plugins::TransactionalTableCommit
);

/// The Duckdb data sink and schema sink: Iceberg on `file://`, read by DuckDB.
pub type DuckdbWriter = IcebergWriter<DuckdbSinkSpec>;

const DUCKDB_WRITE_POLICIES: SinkWritePolicySupport = SinkWritePolicySupport {
    supports_merge_by_key: false,
    supports_replace_partition: false,
    supports_replace_table: true,
};

/// Config for the `Duckdb` data sink and schema sink.
/// `skipprd query` reads filesystem Iceberg ∪ WAL. DuckDB `iceberg_scan` still reads compacted Iceberg only.
#[derive(Debug, Deserialize, Serialize, Clone, SkipprConfig)]
#[serde(deny_unknown_fields)]
pub struct DuckdbConfig {
    /// `file:///abs/path`.
    pub warehouse: String,
    /// Iceberg namespace for sink-managed tables. Unique per warehouse.
    pub table_namespace: String,
}

impl PartialEq for DuckdbConfig {
    fn eq(&self, other: &Self) -> bool {
        skippr_iceberg_catalog::warehouse_key(&self.warehouse)
            == skippr_iceberg_catalog::warehouse_key(&other.warehouse)
            && self.table_namespace == other.table_namespace
    }
}

impl Eq for DuckdbConfig {}

impl DuckdbConfig {
    pub const PLUGIN_NAME: &'static str = sink_capabilities::DUCKDB.name;

    pub fn is_plugin_name(name: &str) -> bool {
        name.eq_ignore_ascii_case(Self::PLUGIN_NAME)
    }

    /// `{warehouse}/{table_namespace}`: the root of every table this sink creates.
    pub fn location_root(&self) -> String {
        format!(
            "{}/{}",
            skippr_iceberg_catalog::warehouse_key(&self.warehouse),
            self.table_namespace
        )
    }

    pub fn validate(&self) -> Result<(), String> {
        skippr_iceberg_catalog::validate_file_warehouse(&self.warehouse, &self.table_namespace)
    }
}

pub fn writer_config(cfg: &DuckdbConfig) -> IcebergWriterConfig {
    IcebergWriterConfig {
        policies: DUCKDB_WRITE_POLICIES,
        table_namespace: cfg.table_namespace.clone(),
        location_root: cfg.location_root(),
    }
}

pub async fn open_fs_catalog(cfg: &DuckdbConfig) -> io::Result<Arc<dyn Catalog>> {
    cfg.validate().map_err(io::Error::other)?;
    let file_io = FileIO::new_with_fs();
    let catalog = FsCatalog::new(cfg.warehouse.clone(), file_io)
        .map_err(|err| io::Error::other(err.to_string()))?;
    Ok(Arc::new(catalog))
}

pub async fn open_writer(
    cfg: &DuckdbConfig,
    context: skippr_runtime_sdk::protocol::RuntimeExecutionContext,
    binding: skippr_runtime_sdk::protocol::RuntimeBinding,
    buffer_name: String,
) -> io::Result<DuckdbWriter> {
    let catalog = open_fs_catalog(cfg).await?;
    DuckdbWriter::new(context, binding, buffer_name, catalog, writer_config(cfg)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_runtime_sdk::plugins::source_contract::{
        validate_write_policy_for_sink, FieldPath, SourceNamespaceContract, WritePolicy,
    };

    fn decode(json: serde_json::Value) -> Result<DuckdbConfig, serde_json::Error> {
        serde_json::from_value(json)
    }

    fn valid_body() -> serde_json::Value {
        serde_json::json!({
            "warehouse": "file:///tmp/lake",
            "table_namespace": "bronze"
        })
    }

    #[test]
    fn duckdb_requires_warehouse_and_table_namespace() {
        for missing in ["warehouse", "table_namespace"] {
            let mut body = valid_body();
            body.as_object_mut().unwrap().remove(missing);
            let err = decode(body).unwrap_err().to_string();
            assert!(
                err.contains("missing field") && err.contains(missing),
                "{missing}: {err}"
            );
        }
        let mut empty = valid_body();
        empty["table_namespace"] = serde_json::json!("");
        let cfg = decode(empty).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("table_namespace"), "{err}");
    }

    #[test]
    fn duckdb_config_rejects_catalog_and_query_engine() {
        for banned in [
            serde_json::json!({"catalog": {"type": "rest"}}),
            serde_json::json!({"query_engine": {"type": "duckdb"}}),
            serde_json::json!({"table_prefix": "skippr"}),
            serde_json::json!({"table_location_prefix": "file:///x"}),
            serde_json::json!({"format": "parquet"}),
            serde_json::json!({"path": "/tmp/lake"}),
            serde_json::json!({"schema": "bronze"}),
            serde_json::json!({"object_store": {"type": "s3"}}),
        ] {
            let mut body = valid_body();
            for (k, v) in banned.as_object().unwrap() {
                body[k] = v.clone();
            }
            let err = decode(body).unwrap_err().to_string();
            assert!(err.contains("unknown field"), "{err}");
        }
    }

    #[test]
    fn duckdb_config_rejects_s3_warehouse() {
        let mut body = valid_body();
        body["warehouse"] = serde_json::json!("s3://bucket/wh");
        let cfg = decode(body).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("file://"), "{err}");
    }

    #[test]
    fn duckdb_config_rejects_bare_absolute_warehouse() {
        let mut body = valid_body();
        body["warehouse"] = serde_json::json!("/tmp/lake");
        let cfg = decode(body).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("file://"), "{err}");
    }

    #[test]
    fn writer_config_derives_location_root_from_warehouse_and_namespace() {
        let cfg = decode(valid_body()).unwrap();
        let writer = writer_config(&cfg);
        assert_eq!(writer.table_namespace, "bronze");
        assert_eq!(writer.location_root, "file:///tmp/lake/bronze");
        assert!(!writer.policies.supports_merge_by_key);
        assert!(!writer.policies.supports_replace_partition);
        assert!(writer.policies.supports_replace_table);
    }

    #[test]
    fn location_root_strips_trailing_slash() {
        let mut body = valid_body();
        body["warehouse"] = serde_json::json!("file:///tmp/lake/");
        let cfg = decode(body).unwrap();
        assert_eq!(writer_config(&cfg).location_root, "file:///tmp/lake/bronze");
    }

    #[test]
    fn location_root_trims_warehouse() {
        let mut body = valid_body();
        body["warehouse"] = serde_json::json!("  file:///tmp/lake/  ");
        let cfg = decode(body).unwrap();
        assert_eq!(writer_config(&cfg).location_root, "file:///tmp/lake/bronze");
        assert_eq!(
            cfg,
            decode(valid_body()).unwrap(),
            "warehouse_key identity, not raw strings"
        );
    }

    #[test]
    fn duckdb_sink_rejects_merge_by_key() {
        let contract = SourceNamespaceContract {
            namespace: "orders".into(),
            write_policy: WritePolicy::MergeByKey,
            primary_key: vec![FieldPath::single("id")],
            cursor: None,
            partition_key: vec![],
            refresh_window: None,
            description: String::new(),
            semantics: None,
        };
        let err = validate_write_policy_for_sink(
            &contract,
            DuckdbConfig::PLUGIN_NAME,
            DUCKDB_WRITE_POLICIES,
        )
        .unwrap_err();
        assert!(err.to_string().contains("MergeByKey"), "{err}");
        assert!(err.to_string().contains("Duckdb"), "{err}");
    }

    #[test]
    fn duckdb_sink_rejects_replace_partition() {
        let contract = SourceNamespaceContract {
            namespace: "orders".into(),
            write_policy: WritePolicy::ReplacePartition,
            primary_key: vec![],
            cursor: None,
            partition_key: vec![FieldPath::single("ds")],
            refresh_window: None,
            description: String::new(),
            semantics: None,
        };
        let err = validate_write_policy_for_sink(
            &contract,
            DuckdbConfig::PLUGIN_NAME,
            DUCKDB_WRITE_POLICIES,
        )
        .unwrap_err();
        assert!(err.to_string().contains("ReplacePartition"), "{err}");
        assert!(err.to_string().contains("Duckdb"), "{err}");
    }

    #[test]
    fn plugin_name_matches_capability() {
        assert_eq!(DuckdbConfig::PLUGIN_NAME, sink_capabilities::DUCKDB.name);
        assert!(DuckdbConfig::is_plugin_name("Duckdb"));
        assert!(DuckdbConfig::is_plugin_name("duckdb"));
        assert!(!DuckdbConfig::is_plugin_name("SkipprLake"));
        assert!(!DuckdbConfig::is_plugin_name("Motherduck"));
    }

    #[test]
    fn duckdb_grouped_exact_once_is_allowed() {
        use skippr_runtime_sdk::plugins::{SinkSpec, SinkWriteSupport};
        assert!(<DuckdbSinkSpec as SinkSpec>::WriteSupport::EXACT_ONCE_ALLOWED);
        assert!(skippr_runtime_sdk::plugins::cdc::sink_capabilities::DUCKDB
            .retry_semantics
            .grouped_writes_are_exact_once());
    }
}
