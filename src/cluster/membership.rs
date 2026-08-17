//! Tables/DynamoDB membership is cold discovery. Gossip is liveness/suspicion.
//! Replica RPC `Status` is commit authority.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

#[cfg(any(
    feature = "offset-store-dynamodb",
    feature = "offset-store-cloud-tables"
))]
use std::collections::HashMap;
#[cfg(any(
    feature = "offset-store-dynamodb",
    feature = "offset-store-cloud-tables"
))]
use std::time::Instant;

use skippr_lease::{Clock, HostId, NodeId, Sleeper, LEASE_RENEW_PERIOD, LEASE_TIMEOUT};
use tokio::sync::RwLock;

#[cfg(any(
    feature = "offset-store-dynamodb",
    feature = "offset-store-cloud-tables"
))]
use skippr_lease::{ClusterId, ClusterMembershipStore, NodeAd};

use crate::cluster::identity::ClusterConfig;

#[derive(Clone, Copy, Debug)]
pub struct MembershipEndpoints {
    pub replica: SocketAddr,
    pub flight: SocketAddr,
    pub gossip: SocketAddr,
}

pub struct MembershipService {
    config: ClusterConfig,
    endpoints: MembershipEndpoints,
    heartbeat: Arc<RwLock<u64>>,
    ready: Arc<RwLock<bool>>,
    #[cfg(any(
        feature = "offset-store-dynamodb",
        feature = "offset-store-cloud-tables"
    ))]
    last_seen: Arc<RwLock<HashMap<String, (u64, Instant)>>>,
    #[cfg(any(
        feature = "offset-store-dynamodb",
        feature = "offset-store-cloud-tables"
    ))]
    store: Option<Arc<dyn ClusterMembershipStore>>,
}

impl MembershipService {
    pub async fn start(
        config: ClusterConfig,
        endpoints: MembershipEndpoints,
    ) -> Result<Self, String> {
        #[cfg(any(
            feature = "offset-store-dynamodb",
            feature = "offset-store-cloud-tables"
        ))]
        let store = if config.table.is_empty() {
            None
        } else {
            Some(crate::cluster::backend::open_membership_store(config.table.clone()).await?)
        };
        Ok(Self {
            config,
            endpoints,
            heartbeat: Arc::new(RwLock::new(1)),
            ready: Arc::new(RwLock::new(false)),
            #[cfg(any(
                feature = "offset-store-dynamodb",
                feature = "offset-store-cloud-tables"
            ))]
            last_seen: Arc::new(RwLock::new(HashMap::new())),
            #[cfg(any(
                feature = "offset-store-dynamodb",
                feature = "offset-store-cloud-tables"
            ))]
            store,
        })
    }

    pub fn config(&self) -> &ClusterConfig {
        &self.config
    }

    pub fn endpoints(&self) -> MembershipEndpoints {
        self.endpoints
    }

    pub async fn set_ready(&self, ready: bool) {
        *self.ready.write().await = ready;
    }

    pub async fn cold_gossip_seeds(&self) -> Vec<SocketAddr> {
        #[cfg(any(
            feature = "offset-store-dynamodb",
            feature = "offset-store-cloud-tables"
        ))]
        if let Some(store) = self.store.as_ref() {
            let Ok(records) = store.query_cluster(&self.config.cluster_id).await else {
                return Vec::new();
            };
            let seeds: Vec<_> = records
                .into_iter()
                .filter(|record| record.ad.node_id != self.config.node_id)
                .map(|record| record.ad.gossip_addr)
                .collect();
            tracing::info!(count = seeds.len(), "cold gossip seeds from membership");
            return seeds;
        }
        Vec::new()
    }

    pub async fn replica_candidates(&self) -> Vec<crate::cluster::placement::ReplicaCandidate> {
        #[cfg(any(
            feature = "offset-store-dynamodb",
            feature = "offset-store-cloud-tables"
        ))]
        if let Some(store) = self.store.as_ref() {
            let Ok(records) = store.query_cluster(&self.config.cluster_id).await else {
                return Vec::new();
            };
            return records
                .into_iter()
                .filter(|record| record.ad.node_id != self.config.node_id)
                .map(|record| crate::cluster::placement::ReplicaCandidate {
                    node_id: record.ad.node_id,
                    host_id: record.ad.host_id,
                    ready: record.ad.ready,
                    protocol_min: record.ad.protocol_min,
                    protocol_max: record.ad.protocol_max,
                    endpoint: record.ad.replica_addr,
                    committed_lag: 0,
                    disk_pressure: record.ad.disk_pressure,
                })
                .collect();
        }
        Vec::new()
    }

    pub async fn tick(&self) {
        let mut hb = self.heartbeat.write().await;
        *hb = hb.saturating_add(1);
        #[cfg(any(
            feature = "offset-store-dynamodb",
            feature = "offset-store-cloud-tables"
        ))]
        if let Some(store) = self.store.as_ref() {
            let ad = NodeAd {
                node_id: self.config.node_id,
                host_id: self.config.host_id.clone(),
                host_label: self.config.host_id.as_str().to_string(),
                replica_addr: self.endpoints.replica,
                flight_addr: self.endpoints.flight,
                gossip_addr: self.endpoints.gossip,
                heartbeat: *hb,
                protocol_min: skippr_lease::PROTOCOL_MIN,
                protocol_max: skippr_lease::PROTOCOL_MAX,
                capabilities: "replica,flight,gossip".into(),
                ready: *self.ready.read().await,
                disk_pressure: crate::cluster::disk::disk_under_pressure(&self.config.data_root),
            };
            if let Err(err) = store.put(&self.config.cluster_id, &ad).await {
                tracing::warn!(error = %err, "membership PutItem failed");
            }
            prune_stale_members(
                store.as_ref(),
                &self.config.cluster_id,
                self.config.node_id,
                &self.last_seen,
            )
            .await;
        }
    }

    pub async fn heartbeat(&self) -> u64 {
        *self.heartbeat.read().await
    }
}

#[cfg(any(
    feature = "offset-store-dynamodb",
    feature = "offset-store-cloud-tables"
))]
pub async fn publish_membership(
    store: &dyn ClusterMembershipStore,
    cluster: &ClusterId,
    ad: &NodeAd,
) -> Result<(), String> {
    store.put(cluster, ad).await.map_err(|err| err.to_string())
}

pub fn same_host(a: &HostId, b: &HostId) -> bool {
    a == b
}

pub fn heartbeat_frozen_long_enough(elapsed: Duration) -> bool {
    elapsed >= LEASE_TIMEOUT
}

pub fn exclude_self(candidates: &[NodeId], self_id: NodeId) -> Vec<NodeId> {
    candidates
        .iter()
        .copied()
        .filter(|id| *id != self_id)
        .collect()
}

#[cfg(any(
    feature = "offset-store-dynamodb",
    feature = "offset-store-cloud-tables"
))]
async fn prune_stale_members(
    store: &dyn ClusterMembershipStore,
    cluster: &ClusterId,
    self_id: NodeId,
    last_seen: &RwLock<HashMap<String, (u64, Instant)>>,
) {
    let Ok(records) = store.query_cluster(cluster).await else {
        return;
    };
    for record in records {
        if record.ad.node_id == self_id {
            continue;
        }
        let id = record.ad.node_id.to_string();
        let heartbeat = record.ad.heartbeat;
        let stale = {
            let mut seen = last_seen.write().await;
            match seen.get(&id).copied() {
                Some((prev, since)) if prev == heartbeat => {
                    heartbeat_frozen_long_enough(since.elapsed())
                }
                _ => {
                    seen.insert(id, (heartbeat, Instant::now()));
                    false
                }
            }
        };
        if !stale {
            continue;
        }
        crate::metrics::counters::add_cluster_membership_dial_fail(1);
        // A SIGSTOP'd process still accepts TCP on the listen backlog. Heartbeat
        // is the membership liveness signal; do not keep a row because connect()
        // succeeded.
        let _ = store
            .delete_if_heartbeat(cluster, &record.ad.node_id, Some(heartbeat))
            .await;
    }
}

pub async fn renew_loop(
    service: Arc<MembershipService>,
    sleeper: Arc<dyn Sleeper>,
    clock: Arc<dyn Clock>,
) {
    loop {
        service.tick().await;
        sleeper
            .sleep_until(clock.monotonic_now().saturating_add(LEASE_RENEW_PERIOD))
            .await;
        let _ = Duration::from_secs(10);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclude_self_drops_local_generation() {
        let a = NodeId::generate();
        let b = NodeId::generate();
        let kept = exclude_self(&[a, b], a);
        assert_eq!(kept, vec![b]);
    }

    #[test]
    fn membership_docs_separate_discovery_from_leases() {
        let src = include_str!("membership.rs");
        assert!(src.contains("cold discovery"));
        assert!(src.contains("commit authority"));
    }

    #[test]
    fn frozen_heartbeat_is_stale_without_tcp() {
        assert!(!heartbeat_frozen_long_enough(
            LEASE_TIMEOUT - Duration::from_secs(1)
        ));
        assert!(heartbeat_frozen_long_enough(LEASE_TIMEOUT));
    }
}
