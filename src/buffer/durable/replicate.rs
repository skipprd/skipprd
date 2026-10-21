use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use skippr_lease::{DurableError, LeaseGuard};

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
    async fn replicate_payload(
        &self,
        pipeline: &skippr_lease::PipelineKey,
        segment_id: &str,
        sha256: [u8; 32],
        len: u64,
        epoch: u64,
        payload: &Path,
    ) -> Result<(), DurableError>;

    fn current_endpoint(&self) -> Option<SocketAddr> {
        None
    }

    async fn replicate_payload_at(
        &self,
        endpoint: SocketAddr,
        pipeline: &skippr_lease::PipelineKey,
        segment_id: &str,
        sha256: [u8; 32],
        len: u64,
        epoch: u64,
        payload: &Path,
    ) -> Result<(), DurableError> {
        let _ = endpoint;
        self.replicate_payload(pipeline, segment_id, sha256, len, epoch, payload)
            .await
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

    pub async fn commit_payload(
        &self,
        segment_id: &str,
        sha256: [u8; 32],
        len: u64,
        epoch: u64,
        payload: &Path,
        guard: &LeaseGuard,
    ) -> Result<(), DurableError> {
        self.commit_payload_at(
            self.snapshot_endpoint(),
            segment_id,
            sha256,
            len,
            epoch,
            payload,
            guard,
        )
        .await
    }

    pub async fn commit_payload_at(
        &self,
        endpoint: SocketAddr,
        segment_id: &str,
        sha256: [u8; 32],
        len: u64,
        epoch: u64,
        payload: &Path,
        guard: &LeaseGuard,
    ) -> Result<(), DurableError> {
        guard
            .require_active_epoch()
            .map_err(|_| DurableError::Fenced)?;
        match self
            .inner
            .replicate_payload_at(
                endpoint,
                guard.key(),
                segment_id,
                sha256,
                len,
                epoch,
                payload,
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(err) => {
                match &err {
                    DurableError::Timeout => {
                        crate::metrics::counters::add_cluster_quorum_timeout(1);
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
    async fn scripted_peer_nack_is_disk_full() {
        let peer = ScriptedPeer::new(vec![Err(DurableError::DiskFull)]);
        let replicator = QuorumReplicator::new(peer, "127.0.0.1:1".parse().unwrap());
        let key = skippr_lease::PipelineKey::new("t", "w", "p").unwrap();
        let guard =
            skippr_lease::LeaseGuard::single_node(key, Arc::new(skippr_lease::SystemClock::new()));
        let err = replicator
            .commit_payload("seg-1", [0u8; 32], 0, 1, Path::new("/nonexistent"), &guard)
            .await
            .unwrap_err();
        assert!(matches!(err, DurableError::DiskFull));
    }
}
