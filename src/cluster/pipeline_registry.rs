//! Pipeline registry watch: tables `cloud-pipelines` is SoT on Cloud.

use skippr_lease::{DurableError, PipelineKey};

use crate::helpers::configuration::Config;

pub async fn scheduled_pipeline_keys() -> Result<Vec<PipelineKey>, DurableError> {
    #[cfg(feature = "offset-store-cloud-tables")]
    {
        if crate::cluster::backend::uses_cloud_tables() {
            return cloud_tables_keys().await;
        }
    }
    yaml_pipeline_keys()
}

fn yaml_pipeline_keys() -> Result<Vec<PipelineKey>, DurableError> {
    let cfg = Config::get();
    let tenant = cfg
        .skippr
        .as_ref()
        .and_then(|s| s.tenant.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::env::var("TENANT").unwrap_or_else(|_| "default".into()));
    let workspace = cfg
        .skippr
        .as_ref()
        .and_then(|s| s.workspace.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::env::var("WORKSPACE_NAME").unwrap_or_else(|_| "default".into()));
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
    use skippr_store_cloud_tables::PipelineRegistry;
    let registry = skippr_store_cloud_tables::CloudTablesPipelineRegistry::connect()
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

#[cfg(all(test, feature = "offset-store-cloud-tables"))]
mod tests {
    use super::*;
    use skippr_store_cloud_tables::{
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
            source: Some("postgres://orders".into()),
            sink: Some("iceberg://lake".into()),
        });
        registry.upsert(PipelineRecord {
            key: PipelineKey::new("globex", "default", "orders").unwrap(),
            kind: PipelineKind::Tenant,
            enabled: true,
            generation: 1,
            source: Some("postgres://orders".into()),
            sink: Some("iceberg://lake".into()),
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
