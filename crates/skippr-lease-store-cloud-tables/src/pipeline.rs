//! Cluster-wide pipeline registry in tables `cloud-pipelines`.
//!
//! Tenant ELT: `PIPE#{tenant}#{workspace}#{name}`. Platform OTel: one shared
//! `PIPE#system#platform#otel-{logs,metrics,traces}` row each.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use skippr_cloud::{attr_n, attr_s, n, s, Client};
use skippr_lease::{LeaseError, PipelineKey};

pub const PIPELINES_TABLE: &str = "cloud-pipelines";
pub const PLATFORM_TENANT: &str = "system";
pub const PLATFORM_WORKSPACE: &str = "platform";
pub const OTEL_LOGS: &str = "otel-logs";
pub const OTEL_METRICS: &str = "otel-metrics";
pub const OTEL_TRACES: &str = "otel-traces";
const META_SK: &str = "META";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipelineKind {
    Tenant,
    Platform,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PipelineRecord {
    pub key: PipelineKey,
    pub kind: PipelineKind,
    pub enabled: bool,
    pub generation: u64,
    pub config: Option<String>,
}

impl PipelineRecord {
    pub fn platform_otel(name: &str) -> Result<Self, LeaseError> {
        Ok(Self {
            key: PipelineKey::new(PLATFORM_TENANT, PLATFORM_WORKSPACE, name)
                .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?,
            kind: PipelineKind::Platform,
            enabled: true,
            generation: 1,
            config: None,
        })
    }

    pub fn pk(key: &PipelineKey) -> String {
        format!(
            "PIPE#{}#{}#{}",
            key.tenant(),
            key.workspace(),
            key.pipeline()
        )
    }
}

#[async_trait]
pub trait PipelineRegistry: Send + Sync {
    async fn list(&self) -> Result<Vec<PipelineRecord>, LeaseError>;
}

#[derive(Default)]
pub struct MemoryPipelineRegistry {
    rows: Mutex<BTreeMap<String, PipelineRecord>>,
}

impl MemoryPipelineRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(&self, record: PipelineRecord) {
        let pk = PipelineRecord::pk(&record.key);
        self.rows.lock().expect("lock").insert(pk, record);
    }
}

#[async_trait]
impl PipelineRegistry for MemoryPipelineRegistry {
    async fn list(&self) -> Result<Vec<PipelineRecord>, LeaseError> {
        Ok(self.rows.lock().expect("lock").values().cloned().collect())
    }
}

pub struct CloudTablesPipelineRegistry {
    client: Arc<Client>,
    table: String,
}

impl CloudTablesPipelineRegistry {
    pub fn new(client: Arc<Client>, table: String) -> Self {
        Self { client, table }
    }

    pub async fn connect() -> Result<Self, LeaseError> {
        let client =
            Client::from_env().map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
        Ok(Self::new(Arc::new(client), PIPELINES_TABLE.to_string()))
    }

    pub async fn ensure_platform_otel(&self) -> Result<(), LeaseError> {
        for name in [OTEL_LOGS, OTEL_METRICS, OTEL_TRACES] {
            let record = PipelineRecord::platform_otel(name)?;
            let item = json!({
                "PK": s(PipelineRecord::pk(&record.key)),
                "SK": s(META_SK),
                "kind": s("platform"),
                "tenant": s(record.key.tenant().to_string()),
                "workspace": s(record.key.workspace().to_string()),
                "name": s(record.key.pipeline().to_string()),
                "enabled": s("true"),
                "generation": n(record.generation),
            });
            if let Err(err) = self
                .client
                .put_item(&self.table, item, Some("attribute_not_exists(PK)"), None)
                .await
            {
                if !err.is_conditional_check_failed() {
                    return Err(LeaseError::StoreUnavailable(err.to_string()));
                }
            }
        }
        Ok(())
    }

    fn decode(item: &serde_json::Value) -> Result<Option<PipelineRecord>, LeaseError> {
        let pk = attr_s(item, "PK").unwrap_or_default();
        if !pk.starts_with("PIPE#") {
            return Ok(None);
        }
        let parts: Vec<&str> = pk.trim_start_matches("PIPE#").split('#').collect();
        if parts.len() != 3 {
            return Ok(None);
        }
        let key = PipelineKey::new(parts[0], parts[1], parts[2])
            .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
        let kind = match attr_s(item, "kind").as_deref() {
            Some("platform") => PipelineKind::Platform,
            _ => PipelineKind::Tenant,
        };
        let enabled = attr_s(item, "enabled")
            .map(|v| v != "false")
            .unwrap_or(true);
        Ok(Some(PipelineRecord {
            key,
            kind,
            enabled,
            generation: attr_n(item, "generation").unwrap_or(1),
            config: attr_s(item, "config"),
        }))
    }
}

#[async_trait]
impl PipelineRegistry for CloudTablesPipelineRegistry {
    async fn list(&self) -> Result<Vec<PipelineRecord>, LeaseError> {
        let items = self
            .client
            .scan(&self.table)
            .await
            .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
        let mut out = Vec::new();
        for item in items {
            if let Some(record) = Self::decode(&item)? {
                if record.enabled {
                    out.push(record);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::{MemoryLeaseStore, NodeId, PipelineLeaseStore};

    #[tokio::test]
    async fn two_tenant_pipelines_and_one_shared_otel_logs() {
        let registry = MemoryPipelineRegistry::new();
        registry.upsert(PipelineRecord {
            key: PipelineKey::new("acme", "default", "ingest").unwrap(),
            kind: PipelineKind::Tenant,
            enabled: true,
            generation: 1,
            config: None,
        });
        registry.upsert(PipelineRecord {
            key: PipelineKey::new("globex", "default", "ingest").unwrap(),
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
                .filter(|r| r.key.pipeline() == OTEL_LOGS)
                .count(),
            1
        );

        let leases = MemoryLeaseStore::new();
        let owner = NodeId::generate();
        for record in &listed {
            leases.create(&record.key, &owner).await.unwrap();
        }
        assert!(leases
            .read_consistent(&listed[0].key)
            .await
            .unwrap()
            .is_some());
        assert!(leases
            .read_consistent(&listed[1].key)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn missing_tables_fails_closed() {
        struct Closed;
        #[async_trait]
        impl PipelineRegistry for Closed {
            async fn list(&self) -> Result<Vec<PipelineRecord>, LeaseError> {
                Err(LeaseError::StoreUnavailable("tables unreachable".into()))
            }
        }
        assert!(Closed.list().await.is_err());
    }
}
