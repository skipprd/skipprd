use skippr_lease::{LeaseSession, PipelineKey, PipelineLeaseStore, ShutdownError};

use crate::cluster::gossip::GossipService;
use crate::cluster::peer::ReplicaServer;
use crate::query_flight::QueryFlightServer;

pub struct LeaseRelease<'a> {
    pub store: &'a dyn PipelineLeaseStore,
    pub key: &'a PipelineKey,
    pub session: &'a LeaseSession,
}

pub async fn shutdown_cluster(
    replica: &ReplicaServer,
    flight: &QueryFlightServer,
    gossip: &GossipService,
    release: Option<LeaseRelease<'_>>,
) -> Result<(), ShutdownError> {
    flight.drain().await;
    crate::query_flight::ballista::drain().await;
    replica.drain().await;
    gossip.drain().await;
    crate::cluster::identity::clear_process_query_bind();
    crate::cluster::peer::clear_process_registry();
    crate::cluster::gossip::clear_gossip();
    if let Some(release) = release {
        release
            .store
            .release_after_drain(release.key, release.session)
            .await
            .map_err(|err| ShutdownError::Other(err.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::identity::ClusterIdentity;
    use crate::cluster::peer::{ReplicaRegistry, ReplicaServer};
    use skippr_lease::{MemoryLeaseStore, NodeId};

    #[tokio::test]
    async fn uncertain_shutdown_does_not_require_release() {
        let identity = ClusterIdentity::new(
            skippr_lease::ClusterId::new("test-cluster").unwrap(),
            NodeId::generate(),
        );
        let replica = ReplicaServer::start_with_registry(
            "127.0.0.1:0".parse().unwrap(),
            ReplicaRegistry::new(identity.clone()),
        )
        .await
        .unwrap();
        let flight = QueryFlightServer::start(
            "127.0.0.1:0".parse().unwrap(),
            ClusterIdentity::new(
                skippr_lease::ClusterId::new("test-cluster").unwrap(),
                NodeId::generate(),
            ),
        )
        .await
        .unwrap();
        let gossip = GossipService::start("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        shutdown_cluster(&replica, &flight, &gossip, None)
            .await
            .unwrap();
        let _ = MemoryLeaseStore::new();
    }
}
