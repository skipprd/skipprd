//! Sled implementation of [`skippr_lease::PipelineLeaseStore`].
//!
//! Process exclusivity is sled's exclusive DB open. Epoch semantics match
//! DynamoDB and Cloud Tables so WAL and engine code stay backend-agnostic.

use async_trait::async_trait;
use skippr_lease::{
    LeaseEpoch, LeaseError, LeaseObservation, LeaseSession, NodeId, PipelineKey, PipelineLeaseStore,
};
use std::sync::Mutex;
use uuid::Uuid;

const TREE: &str = "pipeline_leases";

#[derive(Clone)]
pub struct SledLeaseStore {
    tree: sled::Tree,
    write: std::sync::Arc<Mutex<()>>,
}

impl SledLeaseStore {
    pub fn open(db: &sled::Db) -> Result<Self, String> {
        let tree = db
            .open_tree(TREE)
            .map_err(|err| format!("open pipeline lease tree: {err}"))?;
        Ok(Self {
            tree,
            write: std::sync::Arc::new(Mutex::new(())),
        })
    }

    fn read_row(&self, key: &PipelineKey) -> Result<Option<LeaseObservation>, LeaseError> {
        let bytes = self
            .tree
            .get(key.dynamo_pk().as_bytes())
            .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
        bytes.map(|value| decode(&value)).transpose()
    }

    fn cas(
        &self,
        key: &PipelineKey,
        expected: Option<&LeaseObservation>,
        next: LeaseObservation,
    ) -> Result<LeaseObservation, LeaseError> {
        let _guard = self.write.lock().expect("sled lease lock poisoned");
        let current = self.read_row(key)?;
        match (current.as_ref(), expected) {
            (None, None) => {}
            (Some(current), Some(expected)) if current == expected => {}
            _ => return Err(LeaseError::ConditionalRace),
        }
        self.tree
            .insert(key.dynamo_pk().as_bytes(), encode(&next))
            .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
        Ok(next)
    }
}

fn encode(observation: &LeaseObservation) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + 8 + 8 + 2);
    out.extend_from_slice(observation.owner.as_bytes());
    out.extend_from_slice(&observation.epoch.get().to_le_bytes());
    out.extend_from_slice(&observation.heartbeat.to_le_bytes());
    out.push(u8::from(observation.released));
    out.push(u8::from(observation.initialized));
    out
}

fn decode(bytes: &[u8]) -> Result<LeaseObservation, LeaseError> {
    if bytes.len() != 34 {
        return Err(LeaseError::ProtocolMismatch(format!(
            "sled lease row must be 34 bytes, got {}",
            bytes.len()
        )));
    }
    let uuid = Uuid::from_slice(&bytes[0..16])
        .map_err(|err| LeaseError::ProtocolMismatch(err.to_string()))?;
    let epoch = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let heartbeat = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    Ok(LeaseObservation {
        owner: NodeId::from_uuid(uuid),
        epoch: LeaseEpoch::new(epoch),
        heartbeat,
        released: bytes[32] != 0,
        initialized: bytes[33] != 0,
    })
}

#[async_trait]
impl PipelineLeaseStore for SledLeaseStore {
    async fn create(
        &self,
        key: &PipelineKey,
        owner: &NodeId,
    ) -> Result<LeaseObservation, LeaseError> {
        self.cas(
            key,
            None,
            LeaseObservation {
                owner: *owner,
                epoch: LeaseEpoch::new(1),
                heartbeat: 1,
                released: false,
                initialized: false,
            },
        )
    }

    async fn read_consistent(
        &self,
        key: &PipelineKey,
    ) -> Result<Option<LeaseObservation>, LeaseError> {
        self.read_row(key)
    }

    async fn acquire_released(
        &self,
        key: &PipelineKey,
        observed: &LeaseObservation,
        owner: &NodeId,
    ) -> Result<LeaseObservation, LeaseError> {
        if !observed.released {
            return Err(LeaseError::ConditionalRace);
        }
        self.cas(
            key,
            Some(observed),
            LeaseObservation {
                owner: *owner,
                epoch: observed
                    .epoch
                    .next()
                    .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?,
                heartbeat: 1,
                released: false,
                initialized: observed.initialized,
            },
        )
    }

    async fn steal_unchanged(
        &self,
        key: &PipelineKey,
        observed: &LeaseObservation,
        owner: &NodeId,
    ) -> Result<LeaseObservation, LeaseError> {
        if observed.released {
            return Err(LeaseError::ConditionalRace);
        }
        self.cas(
            key,
            Some(observed),
            LeaseObservation {
                owner: *owner,
                epoch: observed
                    .epoch
                    .next()
                    .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?,
                heartbeat: 1,
                released: false,
                initialized: observed.initialized,
            },
        )
    }

    async fn renew(
        &self,
        key: &PipelineKey,
        session: &LeaseSession,
    ) -> Result<LeaseObservation, LeaseError> {
        let _guard = self.write.lock().expect("sled lease lock poisoned");
        let mut current = self.read_row(key)?.ok_or(LeaseError::Lost)?;
        if current.owner != session.owner || current.epoch != session.epoch || current.released {
            return Err(LeaseError::Lost);
        }
        current.heartbeat = current.heartbeat.saturating_add(1);
        self.tree
            .insert(key.dynamo_pk().as_bytes(), encode(&current))
            .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
        Ok(current)
    }

    async fn mark_initialized(
        &self,
        key: &PipelineKey,
        session: &LeaseSession,
    ) -> Result<(), LeaseError> {
        let _guard = self.write.lock().expect("sled lease lock poisoned");
        let mut current = self.read_row(key)?.ok_or(LeaseError::Lost)?;
        if current.owner != session.owner || current.epoch != session.epoch || current.released {
            return Err(LeaseError::Lost);
        }
        current.initialized = true;
        self.tree
            .insert(key.dynamo_pk().as_bytes(), encode(&current))
            .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
        Ok(())
    }

    async fn release_after_drain(
        &self,
        key: &PipelineKey,
        session: &LeaseSession,
    ) -> Result<(), LeaseError> {
        let _guard = self.write.lock().expect("sled lease lock poisoned");
        let mut current = self.read_row(key)?.ok_or(LeaseError::Lost)?;
        if current.owner != session.owner || current.epoch != session.epoch || current.released {
            return Err(LeaseError::Lost);
        }
        current.released = true;
        current.heartbeat = current.heartbeat.saturating_add(1);
        self.tree
            .insert(key.dynamo_pk().as_bytes(), encode(&current))
            .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::assert_pipeline_lease_conformance;
    use std::sync::Arc;

    #[tokio::test]
    async fn sled_lease_matches_memory_conformance() {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let store = Arc::new(SledLeaseStore::open(&db).unwrap());
        assert_pipeline_lease_conformance(store).await;
    }
}
