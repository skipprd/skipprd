//! Pipeline registry watch: tables `cloud-pipelines` is SoT on Cloud.

use skippr_lease::{DurableError, PipelineKey};

use crate::helpers::configuration::Config;

pub async fn scheduled_pipeline_keys(cfg: &Config) -> Result<Vec<PipelineKey>, DurableError> {
    #[cfg(feature = "offset-store-cloud-tables")]
    {
        if crate::cluster::backend::uses_cloud_tables() {
            return cloud_tables_keys().await;
        }
    }
    yaml_pipeline_keys(cfg)
}

fn yaml_pipeline_keys(cfg: &Config) -> Result<Vec<PipelineKey>, DurableError> {
    let tenant = cfg.get_tenant();
    let workspace = cfg.get_workspace_name();
    let mut keys = Vec::new();
    for name in cfg.pipelines.keys() {
        keys.push(
            PipelineKey::new(&tenant, &workspace, name)
                .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))?,
        );
    }
    Ok(keys)
}

#[cfg(feature = "offset-store-cloud-tables")]
async fn cloud_tables_keys() -> Result<Vec<PipelineKey>, DurableError> {
    use skippr_lease_store_cloud_tables::PipelineRegistry;
    let registry = skippr_lease_store_cloud_tables::CloudTablesPipelineRegistry::connect()
        .await
        .map_err(|err| DurableError::Io(err.to_string()))?;
    registry
        .ensure_platform_otel()
        .await
        .map_err(|err| DurableError::Io(err.to_string()))?;
    let records = registry
        .list()
        .await
        .map_err(|err| DurableError::Io(err.to_string()))?;
    Ok(records.into_iter().map(|record| record.key).collect())
}

#[cfg(test)]
mod yaml_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn yaml_pipeline_keys_use_caller_config() {
        let config: Config = serde_json::from_value(json!({
            "skippr": { "workspace": "ws-a", "tenant": "ten-a" },
            "pipelines": {
                "orders": { "data_source": "data_sources.sample" }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            }
        }))
        .unwrap();
        let keys = yaml_pipeline_keys(&config).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].tenant(), "ten-a");
        assert_eq!(keys[0].workspace(), "ws-a");
        assert_eq!(keys[0].pipeline(), "orders");
    }

    #[test]
    fn yaml_path_does_not_reload_config() {
        let prod = include_str!("pipeline_registry.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(
            !prod.contains("try_build_config"),
            "yaml pipeline keys must use Session-owned Config"
        );
    }
}

#[cfg(all(test, feature = "offset-store-cloud-tables"))]
mod tests {
    use super::*;
    use skippr_lease_store_cloud_tables::{
        MemoryPipelineRegistry, PipelineKind, PipelineRecord, PipelineRegistry, OTEL_LOGS,
    };

    #[tokio::test]
    async fn registry_lists_two_tenants_and_shared_otel() {
        let registry = MemoryPipelineRegistry::new();
        registry.upsert(PipelineRecord {
            key: PipelineKey::new("acme", "default", "orders").unwrap(),
            kind: PipelineKind::Tenant,
            enabled: true,
            generation: 1,
            config: None,
        });
        registry.upsert(PipelineRecord {
            key: PipelineKey::new("globex", "default", "orders").unwrap(),
            kind: PipelineKind::Tenant,
            enabled: true,
            generation: 1,
            config: None,
        });
        registry.upsert(PipelineRecord::platform_otel(OTEL_LOGS).unwrap());
        let listed = registry.list().await.unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(
            listed
                .iter()
                .filter(|r| r.key.tenant() == "system" && r.key.pipeline() == OTEL_LOGS)
                .count(),
            1
        );
    }
}
