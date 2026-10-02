use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use iceberg::Catalog;
use iceberg::CatalogBuilder;
use iceberg_catalog_glue::{
    GlueCatalogBuilder, AWS_REGION_NAME, GLUE_CATALOG_PROP_CATALOG_ID, GLUE_CATALOG_PROP_WAREHOUSE,
};
use serde_derive::{Deserialize, Serialize};
use skippr_iceberg_catalog::WarehouseObjectStore;
pub use skippr_iceberg_writer::{IcebergWriter, IcebergWriterConfig};
use skippr_runtime_sdk::plugins::cdc::sink_capabilities;
use skippr_runtime_sdk::plugins::source_contract::SinkWritePolicySupport;
use skippr_runtime_sdk::SkipprConfig;

skippr_runtime_sdk::declare_sink_spec!(
    AthenaIcebergSinkSpec,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::ATHENA_ICEBERG,
    skippr_runtime_sdk::plugins::TransactionalTableCommit
);

/// The AthenaIceberg data sink and schema sink: Iceberg on S3 via Glue, queried by Athena.
pub type AthenaIcebergWriter = IcebergWriter<AthenaIcebergSinkSpec>;

const ATHENA_ICEBERG_WRITE_POLICIES: SinkWritePolicySupport = SinkWritePolicySupport {
    supports_merge_by_key: true,
    supports_replace_partition: true,
    supports_replace_table: true,
};

/// Config for the `AthenaIceberg` data sink and schema sink.
/// `skipprd query` does not read this sink; that path is WalOnly.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, SkipprConfig)]
#[serde(deny_unknown_fields)]
pub struct AthenaIcebergConfig {
    /// Table storage root: `s3://bucket/prefix/`.
    pub warehouse: String,
    /// Glue database. Also the Iceberg namespace.
    pub glue_database_name: String,
    pub athena_workgroup_name: String,
    /// Bucket name only (not an `s3://` URI), same as `Athena:`.
    pub athena_results_s3_bucket: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub catalog_id: Option<String>,
    #[serde(default)]
    pub object_store: WarehouseObjectStore,
}

impl AthenaIcebergConfig {
    pub const PLUGIN_NAME: &'static str = sink_capabilities::ATHENA_ICEBERG.name;

    pub fn is_plugin_name(name: &str) -> bool {
        name.eq_ignore_ascii_case(Self::PLUGIN_NAME)
    }

    /// `{warehouse}/{glue_database_name}`: the root of every table this sink creates.
    pub fn location_root(&self) -> String {
        format!(
            "{}/{}",
            self.warehouse.trim_end_matches('/'),
            self.glue_database_name
        )
    }

    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("warehouse", self.warehouse.as_str()),
            ("glue_database_name", self.glue_database_name.as_str()),
            ("athena_workgroup_name", self.athena_workgroup_name.as_str()),
            (
                "athena_results_s3_bucket",
                self.athena_results_s3_bucket.as_str(),
            ),
        ] {
            if value.trim().is_empty() {
                return Err(format!("{name} is required"));
            }
        }
        Ok(())
    }
}

pub fn writer_config(cfg: &AthenaIcebergConfig) -> IcebergWriterConfig {
    IcebergWriterConfig {
        policies: ATHENA_ICEBERG_WRITE_POLICIES,
        table_namespace: cfg.glue_database_name.clone(),
        location_root: cfg.location_root(),
    }
}

pub async fn open_glue_catalog(cfg: &AthenaIcebergConfig) -> io::Result<Arc<dyn Catalog>> {
    cfg.validate().map_err(io::Error::other)?;
    let mut props = HashMap::new();
    props.insert(
        GLUE_CATALOG_PROP_WAREHOUSE.to_string(),
        cfg.warehouse.clone(),
    );
    if let Some(id) = &cfg.catalog_id {
        if !id.trim().is_empty() {
            props.insert(GLUE_CATALOG_PROP_CATALOG_ID.to_string(), id.clone());
        }
    }
    if let Some(region) = &cfg.region {
        if !region.trim().is_empty() {
            props.insert(AWS_REGION_NAME.to_string(), region.clone());
        }
    }
    for (k, v) in skippr_iceberg_catalog::s3_object_store_props(&cfg.object_store)
        .map_err(io::Error::other)?
    {
        props.insert(k, v);
    }
    let catalog = GlueCatalogBuilder::default()
        .load("glue", props)
        .await
        .map_err(|err| io::Error::other(err.to_string()))?;
    Ok(Arc::new(catalog))
}

pub async fn open_writer(
    cfg: &AthenaIcebergConfig,
    context: skippr_runtime_sdk::protocol::RuntimeExecutionContext,
    binding: skippr_runtime_sdk::protocol::RuntimeBinding,
    buffer_name: String,
) -> io::Result<AthenaIcebergWriter> {
    let catalog = open_glue_catalog(cfg).await?;
    AthenaIcebergWriter::new(context, binding, buffer_name, catalog, writer_config(cfg)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(json: serde_json::Value) -> Result<AthenaIcebergConfig, serde_json::Error> {
        serde_json::from_value(json)
    }

    fn valid_body() -> serde_json::Value {
        serde_json::json!({
            "warehouse": "s3://lake/warehouse/",
            "glue_database_name": "analytics",
            "athena_workgroup_name": "primary",
            "athena_results_s3_bucket": "athena-results"
        })
    }

    #[test]
    fn athena_iceberg_requires_glue_database_workgroup_and_results_bucket() {
        for missing in [
            "glue_database_name",
            "athena_workgroup_name",
            "athena_results_s3_bucket",
            "warehouse",
        ] {
            let mut body = valid_body();
            body.as_object_mut().unwrap().remove(missing);
            let err = decode(body).unwrap_err().to_string();
            assert!(
                err.contains("missing field") && err.contains(missing),
                "{missing}: {err}"
            );
        }
        let mut empty = valid_body();
        empty["glue_database_name"] = serde_json::json!("");
        let cfg = decode(empty).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("glue_database_name"), "{err}");
    }

    #[test]
    fn athena_iceberg_config_rejects_catalog_type_and_query_engine() {
        for banned in [
            serde_json::json!({"catalog": {"type": "glue"}}),
            serde_json::json!({"query_engine": {"type": "athena"}}),
            serde_json::json!({"table_prefix": "skippr"}),
            serde_json::json!({"table_location_prefix": "s3://x"}),
            serde_json::json!({"format": "parquet"}),
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
    fn writer_config_derives_location_root_from_warehouse_and_glue_database() {
        let cfg = decode(valid_body()).unwrap();
        let writer = writer_config(&cfg);
        assert_eq!(writer.table_namespace, "analytics");
        assert_eq!(writer.location_root, "s3://lake/warehouse/analytics");
    }

    #[test]
    fn plugin_name_matches_capability() {
        assert_eq!(
            AthenaIcebergConfig::PLUGIN_NAME,
            sink_capabilities::ATHENA_ICEBERG.name
        );
        assert!(AthenaIcebergConfig::is_plugin_name("AthenaIceberg"));
        assert!(AthenaIcebergConfig::is_plugin_name("athenaiceberg"));
        assert!(!AthenaIcebergConfig::is_plugin_name("Athena"));
        assert!(!AthenaIcebergConfig::is_plugin_name("SkipprLake"));
        assert!(!AthenaIcebergConfig::is_plugin_name("Iceberg"));
    }

    #[test]
    fn omitted_optional_fields_equal_explicit_nulls() {
        let omitted = decode(valid_body()).unwrap();
        let mut with_nulls = valid_body();
        with_nulls["region"] = serde_json::Value::Null;
        with_nulls["catalog_id"] = serde_json::Value::Null;
        let explicit = decode(with_nulls).unwrap();
        assert_eq!(omitted, explicit);
    }
}
