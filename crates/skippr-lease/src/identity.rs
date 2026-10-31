use crate::error::PathError;
use crate::paths::validate_path_component;
use std::fmt;
use uuid::Uuid;

#[derive(
    Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
pub struct LeaseEpoch(u64);

impl LeaseEpoch {
    pub const SINGLE_NODE: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn next(self) -> Result<Self, PathError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or_else(|| PathError::Overflow("lease epoch overflow".into()))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct NodeId(Uuid);

impl NodeId {
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn single_node() -> Self {
        Self(Uuid::nil())
    }

    pub fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    pub fn as_uuid(self) -> Uuid {
        self.0
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PipelineKey {
    tenant: String,
    workspace: String,
    pipeline: String,
}

impl PipelineKey {
    pub fn new(
        tenant: impl Into<String>,
        workspace: impl Into<String>,
        pipeline: impl Into<String>,
    ) -> Result<Self, PathError> {
        let tenant = tenant.into();
        let workspace = workspace.into();
        let pipeline = pipeline.into();
        validate_path_component(&tenant)?;
        validate_path_component(&workspace)?;
        validate_path_component(&pipeline)?;
        Ok(Self {
            tenant,
            workspace,
            pipeline,
        })
    }

    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    pub fn pipeline(&self) -> &str {
        &self.pipeline
    }

    pub fn dynamo_pk(&self) -> String {
        format!("{}#{}#{}", self.tenant, self.workspace, self.pipeline)
    }

    pub fn pipe_pk(&self) -> String {
        format!("PIPE#{}#{}#{}", self.tenant, self.workspace, self.pipeline)
    }

    pub fn parse_pipe_pk(pk: &str) -> Result<Self, PathError> {
        let rest = pk.strip_prefix("PIPE#").ok_or_else(|| {
            PathError::InvalidComponent(format!("pipeline META PK must start with PIPE#: {pk}"))
        })?;
        let mut parts = rest.split('#');
        let tenant = parts.next().unwrap_or("");
        let workspace = parts.next().unwrap_or("");
        let pipeline = parts.next().unwrap_or("");
        if parts.next().is_some() {
            return Err(PathError::InvalidComponent(format!(
                "pipeline META PK must be PIPE#tenant#workspace#name: {pk}"
            )));
        }
        Self::new(tenant, workspace, pipeline)
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(self.tenant.as_bytes());
        out.push(0);
        out.extend_from_slice(self.workspace.as_bytes());
        out.push(0);
        out.extend_from_slice(self.pipeline.as_bytes());
        out
    }
}

#[derive(
    Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
pub struct SegmentId(String);

impl SegmentId {
    pub fn new(raw: impl Into<String>) -> Result<Self, PathError> {
        let value = raw.into();
        validate_path_component(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_key_does_not_encode_cluster_membership() {
        let key = PipelineKey::new("acme", "prod", "events").unwrap();
        assert_eq!(key.dynamo_pk(), "acme#prod#events");
        assert!(!key.dynamo_pk().starts_with("cluster#"));
        assert!(!key.dynamo_pk().ends_with("#cluster"));
    }

    #[test]
    fn pipeline_key_pipe_pk_roundtrips_and_rejects_hash() {
        let key = PipelineKey::new("acme", "default", "orders").unwrap();
        assert_eq!(key.pipe_pk(), "PIPE#acme#default#orders");
        assert_eq!(PipelineKey::parse_pipe_pk(&key.pipe_pk()).unwrap(), key);
        assert!(PipelineKey::new("acme#x", "default", "orders").is_err());
        assert!(PipelineKey::parse_pipe_pk("acme#default#orders").is_err());
        assert!(PipelineKey::parse_pipe_pk("PIPE#acme#default#orders#extra").is_err());
        assert!(PipelineKey::parse_pipe_pk("RUN#t#r").is_err());
    }
}
