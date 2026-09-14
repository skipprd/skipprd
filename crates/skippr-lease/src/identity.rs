use crate::clock::MonoInstant;
use crate::error::PathError;
use crate::paths::validate_path_component;
use std::fmt;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CommitIndex(u64);

impl CommitIndex {
    pub const ZERO: Self = Self(0);

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
            .ok_or_else(|| PathError::Overflow("commit index overflow".into()))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NodeId(Uuid);

impl NodeId {
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
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
pub struct HostId(String);

impl HostId {
    pub fn new(raw: impl Into<String>) -> Result<Self, PathError> {
        let value = raw.into();
        if value.trim().is_empty() {
            return Err(PathError::InvalidComponent(
                "host id must not be empty".into(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ClusterId(String);

impl ClusterId {
    pub fn new(raw: impl Into<String>) -> Result<Self, PathError> {
        let value = raw.into();
        validate_path_component(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn membership_pk(&self) -> String {
        format!("cluster#{}", self.0)
    }

    pub fn hash_hex(&self) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(self.0.as_bytes());
        hex_lower(&digest)
    }
}

impl fmt::Display for ClusterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
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
        for component in [&tenant, &workspace, &pipeline] {
            if component.contains('#') {
                return Err(PathError::InvalidComponent(
                    "pipeline key component must not contain '#'".into(),
                ));
            }
        }
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

#[derive(Clone, Debug, Eq, PartialEq)]
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeaseObservation {
    pub owner: NodeId,
    pub epoch: LeaseEpoch,
    pub heartbeat: u64,
    pub released: bool,
    pub initialized: bool,
}

#[derive(Clone, Debug)]
pub struct LeaseSession {
    pub owner: NodeId,
    pub epoch: LeaseEpoch,
    pub heartbeat: u64,
    pub initialized: bool,
    pub local_deadline: MonoInstant,
}

impl LeaseSession {
    pub fn from_observation(observation: LeaseObservation, local_deadline: MonoInstant) -> Self {
        Self {
            owner: observation.owner,
            epoch: observation.epoch,
            heartbeat: observation.heartbeat,
            initialized: observation.initialized,
            local_deadline,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_id_membership_pk_is_not_tenant_scoped() {
        let id = ClusterId::new("lake-a").unwrap();
        assert_eq!(id.membership_pk(), "cluster#lake-a");
        assert_ne!(id.hash_hex(), ClusterId::new("lake-b").unwrap().hash_hex());
    }

    #[test]
    fn pipeline_key_does_not_encode_membership() {
        let key = PipelineKey::new("acme", "prod", "events").unwrap();
        assert_eq!(key.dynamo_pk(), "acme#prod#events");
        assert_ne!(
            key.dynamo_pk(),
            ClusterId::new("acme").unwrap().membership_pk()
        );
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
