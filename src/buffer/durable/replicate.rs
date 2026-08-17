use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use skippr_lease::{DurableError, LeaseGuard};

use super::mutation::MutationEnvelope;

pub enum ReplicationMode {
    LocalOnly,
    Synchronous(Arc<QuorumReplicator>),
}

pub struct QuorumReplicator {
    inner: Arc<dyn ReplicaClient>,
    assigned: SocketAddr,
}

#[async_trait::async_trait]
pub trait ReplicaClient: Send + Sync {
    async fn replicate(
        &self,
        envelope: &MutationEnvelope,
        payload: &Path,
    ) -> Result<(), DurableError>;

    fn current_endpoint(&self) -> Option<SocketAddr> {
        None
    }

    async fn replicate_at(
        &self,
        endpoint: SocketAddr,
        envelope: &MutationEnvelope,
        payload: &Path,
    ) -> Result<(), DurableError> {
        let _ = endpoint;
        self.replicate(envelope, payload).await
    }
}

impl QuorumReplicator {
    pub fn new(inner: Arc<dyn ReplicaClient>, assigned: SocketAddr) -> Arc<Self> {
        Arc::new(Self { inner, assigned })
    }

    pub fn assigned(&self) -> SocketAddr {
        self.assigned
    }

    pub fn snapshot_endpoint(&self) -> SocketAddr {
        self.inner.current_endpoint().unwrap_or(self.assigned)
    }

    pub async fn commit(
        &self,
        envelope: &MutationEnvelope,
        payload: &Path,
        guard: &LeaseGuard,
    ) -> Result<(), DurableError> {
        self.commit_at(self.snapshot_endpoint(), envelope, payload, guard)
            .await
    }

    pub async fn commit_at(
        &self,
        endpoint: SocketAddr,
        envelope: &MutationEnvelope,
        payload: &Path,
        guard: &LeaseGuard,
    ) -> Result<(), DurableError> {
        guard
            .require_active_epoch()
            .map_err(|_| DurableError::Fenced)?;
        match self.inner.replicate_at(endpoint, envelope, payload).await {
            Ok(()) => Ok(()),
            Err(err) => {
                match &err {
                    DurableError::Timeout => {
                        crate::metrics::counters::add_cluster_quorum_timeout(1);
                    }
                    DurableError::Diverged(_) => {
                        crate::metrics::counters::add_cluster_quorum_nack(1);
                        crate::metrics::counters::add_cluster_divergence(1);
                    }
                    _ => crate::metrics::counters::add_cluster_quorum_nack(1),
                }
                crate::metrics::counters::add_cluster_quorum_lost(1);
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::peer::ScriptedPeer;

    #[tokio::test]
    async fn local_only_does_not_require_network() {
        match ReplicationMode::LocalOnly {
            ReplicationMode::LocalOnly => {}
            ReplicationMode::Synchronous(_) => panic!("expected local"),
        }
    }

    #[tokio::test]
    async fn scripted_peer_nack_is_diverged() {
        let peer = ScriptedPeer::new(vec![Err(DurableError::Diverged("x".into()))]);
        let replicator = QuorumReplicator::new(peer, "127.0.0.1:1".parse().unwrap());
        let key = skippr_lease::PipelineKey::new("t", "w", "p").unwrap();
        let guard =
            skippr_lease::LeaseGuard::single_node(key, Arc::new(skippr_lease::SystemClock::new()));
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: skippr_lease::PipelineKey::new("t", "w", "p").unwrap(),
            epoch: skippr_lease::LeaseEpoch::new(0),
            index: skippr_lease::CommitIndex::new(1),
            previous_hash: skippr_lease::GENESIS_HASH,
            payload_sha256: [0u8; 32],
            body: crate::buffer::durable::mutation::DurableMutation::ReclaimSegment {
                segment_id: "s".into(),
            },
        };
        let err = replicator
            .commit(&envelope, Path::new("/nonexistent"), &guard)
            .await
            .unwrap_err();
        assert!(matches!(err, DurableError::Diverged(_)));
    }
}
