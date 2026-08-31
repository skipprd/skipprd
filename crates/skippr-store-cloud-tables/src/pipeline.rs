//! Cluster-wide pipeline registry in tables `cloud-pipelines`.
//!
//! Tenant ELT: `PIPE#{tenant}#{workspace}#{name}`. Platform OTel: one shared
//! `PIPE#system#platform#otel-{logs,metrics,traces}` row each.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use skippr_lease::{LeaseError, PipelineKey};
use skippr_tables_client::{attr_n, attr_s, n, s, TablesClient};

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

impl PipelineKind {
    pub fn as_attr(self) -> &'static str {
        match self {
            Self::Tenant => "tenant",
            Self::Platform => "platform",
        }
    }

    pub fn parse_attr(value: &str) -> Result<Self, LeaseError> {
        match value {
            "tenant" => Ok(Self::Tenant),
            "platform" => Ok(Self::Platform),
            other => Err(LeaseError::ProtocolMismatch(format!(
                "unknown pipeline kind {other}"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PipelineRecord {
    pub key: PipelineKey,
    pub kind: PipelineKind,
    pub enabled: bool,
    pub generation: u64,
    pub source: Option<String>,
    pub sink: Option<String>,
}

impl PipelineRecord {
    pub fn platform_otel(name: &str) -> Result<Self, LeaseError> {
        Ok(Self {
            key: PipelineKey::new(PLATFORM_TENANT, PLATFORM_WORKSPACE, name)
                .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?,
            kind: PipelineKind::Platform,
            enabled: true,
            generation: 1,
            source: None,
            sink: None,
        })
    }

    pub fn pk(key: &PipelineKey) -> String {
        key.pipe_pk()
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
    client: Arc<TablesClient>,
    table: String,
}

impl CloudTablesPipelineRegistry {
    pub fn new(client: Arc<TablesClient>, table: String) -> Self {
        Self { client, table }
    }

    pub async fn connect() -> Result<Self, LeaseError> {
        let client = TablesClient::from_env()
            .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
        Ok(Self::new(Arc::new(client), PIPELINES_TABLE.to_string()))
    }

    pub async fn ensure_platform_otel(&self) -> Result<(), LeaseError> {
        for name in [OTEL_LOGS, OTEL_METRICS, OTEL_TRACES] {
            let record = PipelineRecord::platform_otel(name)?;
            let item = json!({
                "PK": s(PipelineRecord::pk(&record.key)),
                "SK": s(META_SK),
                "kind": s(record.kind.as_attr()),
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
        let key = PipelineKey::parse_pipe_pk(&pk)
            .map_err(|err| LeaseError::ProtocolMismatch(err.to_string()))?;
        let kind = PipelineKind::parse_attr(
            attr_s(item, "kind")
                .ok_or_else(|| LeaseError::ProtocolMismatch("pipeline META missing kind".into()))?
                .as_str(),
        )?;
        let enabled = match attr_s(item, "enabled").as_deref() {
            Some("true") => true,
            Some("false") => false,
            Some(other) => {
                return Err(LeaseError::ProtocolMismatch(format!(
                    "pipeline META enabled must be true or false, not {other}"
                )))
            }
            None => {
                return Err(LeaseError::ProtocolMismatch(
                    "pipeline META missing enabled".into(),
                ))
            }
        };
        let generation = attr_n(item, "generation").ok_or_else(|| {
            LeaseError::ProtocolMismatch("pipeline META missing generation".into())
        })?;
        let source = attr_s(item, "source");
        let sink = attr_s(item, "sink");
        if kind == PipelineKind::Tenant && (source.is_none() || sink.is_none()) {
            return Err(LeaseError::ProtocolMismatch(
                "tenant pipeline META missing source or sink".into(),
            ));
        }
        Ok(Some(PipelineRecord {
            key,
            kind,
            enabled,
            generation,
            source,
            sink,
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
            source: Some("postgres://orders".into()),
            sink: Some("iceberg://lake".into()),
        });
        registry.upsert(PipelineRecord {
            key: PipelineKey::new("globex", "default", "ingest").unwrap(),
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

    #[test]
    fn decode_cloud_tenant_meta_and_rejects_unknown_kind() {
        let key = PipelineKey::new("acme", "default", "orders").unwrap();
        let item = json!({
            "PK": s(key.pipe_pk()),
            "SK": s("META"),
            "kind": s("tenant"),
            "name": s("orders"),
            "workspace": s("default"),
            "source": s("postgres://orders"),
            "sink": s("iceberg://lake"),
            "enabled": s("true"),
            "generation": n(1),
        });
        let record = CloudTablesPipelineRegistry::decode(&item)
            .unwrap()
            .expect("tenant META");
        assert_eq!(record.key, key);
        assert_eq!(record.kind, PipelineKind::Tenant);
        assert_eq!(record.source.as_deref(), Some("postgres://orders"));
        assert_eq!(record.sink.as_deref(), Some("iceberg://lake"));

        let bad_kind = json!({
            "PK": s(key.pipe_pk()),
            "SK": s("META"),
            "kind": s("legacy"),
            "enabled": s("true"),
            "generation": n(1),
        });
        assert!(CloudTablesPipelineRegistry::decode(&bad_kind).is_err());
    }
}
