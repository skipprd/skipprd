use std::collections::HashMap;
use std::net::SocketAddr;

use skippr_lease::{NodeId, PipelineKey};

use crate::cluster::gossip::{GossipAd, WalHeadHint};
use crate::cluster::identity::ClusterIdentity;
use crate::cluster::peer::{query_status, ReplicaRegistry};

pub const WAL_HEAD_CAP: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalCandidate {
    pub node_id: NodeId,
    pub flight: SocketAddr,
    pub replica: SocketAddr,
    pub ready: bool,
    pub committed_index: Option<u64>,
    pub is_local: bool,
}

pub fn cap_wal_heads(mut heads: Vec<WalHeadHint>) -> Vec<WalHeadHint> {
    heads.truncate(WAL_HEAD_CAP);
    heads
}

pub async fn collect_local_wal_heads(registry: Option<&ReplicaRegistry>) -> Vec<WalHeadHint> {
    let mut heads: HashMap<String, u64> = HashMap::new();
    for store in crate::buffer::durable::all_durable_stores() {
        let idx = store.durable_state().await.committed_index.get();
        heads.insert(store.key().pipeline().to_string(), idx);
    }
    if let Some(registry) = registry {
        for session in registry.snapshot().await {
            let pipeline = session.key.pipeline().to_string();
            let idx = session.status().await.committed_index;
            heads
                .entry(pipeline)
                .and_modify(|current| *current = (*current).max(idx))
                .or_insert(idx);
        }
    }
    let mut out: Vec<WalHeadHint> = heads
        .into_iter()
        .map(|(pipeline, committed_index)| WalHeadHint {
            pipeline,
            committed_index,
        })
        .collect();
    out.sort_by(|a, b| a.pipeline.cmp(&b.pipeline));
    cap_wal_heads(out)
}

pub fn candidates_for_pipeline(
    pipeline: &str,
    local_flight: SocketAddr,
    local_node: NodeId,
    local_committed: Option<u64>,
    ads: &[GossipAd],
) -> Vec<WalCandidate> {
    let mut out: Vec<WalCandidate> = ads
        .iter()
        .filter(|ad| ad.ready)
        .map(|ad| {
            let committed = ad
                .wal_heads
                .iter()
                .find(|hint| hint.pipeline == pipeline)
                .map(|hint| hint.committed_index);
            WalCandidate {
                node_id: ad.node_id,
                flight: ad.flight,
                replica: ad.replica,
                ready: ad.ready,
                committed_index: committed,
                is_local: ad.flight == local_flight || ad.node_id == local_node,
            }
        })
        .collect();
    if let Some(idx) = local_committed {
        match out.iter_mut().find(|cand| cand.is_local) {
            Some(local) => {
                local.committed_index = Some(local.committed_index.unwrap_or(0).max(idx));
            }
            None => out.push(WalCandidate {
                node_id: local_node,
                flight: local_flight,
                replica: local_flight,
                ready: true,
                committed_index: Some(idx),
                is_local: true,
            }),
        }
    }
    out
}

pub fn rank_wal_candidates(mut cands: Vec<WalCandidate>) -> Vec<WalCandidate> {
    cands.retain(|cand| cand.ready && cand.committed_index.is_some());
    cands.sort_by(|a, b| {
        b.committed_index
            .cmp(&a.committed_index)
            .then_with(|| b.is_local.cmp(&a.is_local))
            .then_with(|| a.node_id.to_string().cmp(&b.node_id.to_string()))
    });
    cands
}

pub async fn pick_wal_endpoint<F, Fut>(
    pipeline: &PipelineKey,
    local_flight: SocketAddr,
    local_node: NodeId,
    local_committed: Option<u64>,
    ads: &[GossipAd],
    mut confirm: F,
) -> Option<SocketAddr>
where
    F: FnMut(SocketAddr) -> Fut,
    Fut: std::future::Future<Output = Option<u64>>,
{
    let mut cands = candidates_for_pipeline(
        pipeline.pipeline(),
        local_flight,
        local_node,
        local_committed,
        ads,
    );
    if !cands.iter().any(|cand| cand.committed_index.is_some()) {
        for cand in cands.iter_mut().filter(|cand| cand.ready && !cand.is_local) {
            cand.committed_index = confirm(cand.replica).await;
        }
        if let Some(idx) = local_committed {
            for cand in &mut cands {
                if cand.is_local {
                    cand.committed_index = Some(idx);
                }
            }
        }
    }
    for cand in rank_wal_candidates(cands) {
        if cand.is_local && local_committed.is_none() {
            continue;
        }
        // WAL is Flight SQL to `flight`, not replica RPC. A compaction hold can
        // stall replica Status while DoGet on `flight_addr` still serves live WAL.
        return Some(cand.flight);
    }
    None
}

pub async fn confirm_replica_head(
    replica: SocketAddr,
    pipeline: &PipelineKey,
    identity: &ClusterIdentity,
) -> Option<u64> {
    query_status(replica, pipeline, identity)
        .await
        .ok()
        .map(|status| status.committed_index)
}

pub async fn local_wal_paths(
    pipeline: &PipelineKey,
    registry: Option<&ReplicaRegistry>,
) -> Option<skippr_lease::PipelinePaths> {
    if let Some(store) = crate::buffer::durable::durable_store_for(pipeline) {
        return Some(store.paths().clone());
    }
    if let Some(registry) = registry {
        if let Some(session) = registry.get(pipeline).await {
            return Some(session.paths.clone());
        }
    }
    None
}

pub async fn local_committed_for(
    pipeline: &PipelineKey,
    registry: Option<&ReplicaRegistry>,
) -> Option<u64> {
    if let Some(store) = crate::buffer::durable::durable_store_for(pipeline) {
        return Some(store.durable_state().await.committed_index.get());
    }
    if let Some(registry) = registry {
        if let Some(session) = registry.get(pipeline).await {
            return Some(session.status().await.committed_index);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::gossip::GossipAd;
    use crate::cluster::membership::MembershipEndpoints;
    use skippr_lease::HostId;

    fn ad(
        node: NodeId,
        flight: SocketAddr,
        replica: SocketAddr,
        committed: Option<u64>,
        pipeline: &str,
    ) -> GossipAd {
        let mut ad = GossipAd::for_node(
            node,
            HostId::new("host").unwrap(),
            MembershipEndpoints {
                replica,
                flight,
                gossip: replica,
            },
            true,
        );
        if let Some(committed_index) = committed {
            ad.wal_heads = vec![WalHeadHint {
                pipeline: pipeline.into(),
                committed_index,
            }];
        }
        ad
    }

    #[test]
    fn local_wins_tie() {
        let local_node = NodeId::generate();
        let peer = NodeId::generate();
        let local_flight: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let ads = vec![
            ad(
                local_node,
                local_flight,
                "127.0.0.1:1".parse().unwrap(),
                Some(5),
                "p",
            ),
            ad(
                peer,
                "127.0.0.1:10".parse().unwrap(),
                "127.0.0.1:2".parse().unwrap(),
                Some(5),
                "p",
            ),
        ];
        let ranked = rank_wal_candidates(candidates_for_pipeline(
            "p",
            local_flight,
            local_node,
            Some(5),
            &ads,
        ));
        assert!(ranked[0].is_local);
        assert_eq!(ranked[0].flight, local_flight);
    }

    #[test]
    fn higher_peer_wins() {
        let local_node = NodeId::generate();
        let peer = NodeId::generate();
        let local_flight: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let peer_flight: SocketAddr = "127.0.0.1:10".parse().unwrap();
        let ads = vec![
            ad(
                local_node,
                local_flight,
                "127.0.0.1:1".parse().unwrap(),
                Some(5),
                "p",
            ),
            ad(
                peer,
                peer_flight,
                "127.0.0.1:2".parse().unwrap(),
                Some(9),
                "p",
            ),
        ];
        let ranked = rank_wal_candidates(candidates_for_pipeline(
            "p",
            local_flight,
            local_node,
            Some(5),
            &ads,
        ));
        assert!(!ranked[0].is_local);
        assert_eq!(ranked[0].flight, peer_flight);
    }

    #[tokio::test]
    async fn missing_hint_falls_back_to_status() {
        let local_node = NodeId::generate();
        let peer = NodeId::generate();
        let local_flight: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let peer_flight: SocketAddr = "127.0.0.1:10".parse().unwrap();
        let peer_replica: SocketAddr = "127.0.0.1:2".parse().unwrap();
        let ads = vec![ad(peer, peer_flight, peer_replica, None, "p")];
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let picked = pick_wal_endpoint(
            &key,
            local_flight,
            local_node,
            None,
            &ads,
            |replica| async move {
                if replica == peer_replica {
                    Some(3)
                } else {
                    None
                }
            },
        )
        .await;
        assert_eq!(picked, Some(peer_flight));
    }

    #[tokio::test]
    async fn all_status_fail_returns_none() {
        let local_node = NodeId::generate();
        let peer = NodeId::generate();
        let local_flight: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let ads = vec![ad(
            peer,
            "127.0.0.1:10".parse().unwrap(),
            "127.0.0.1:2".parse().unwrap(),
            None,
            "p",
        )];
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let picked = pick_wal_endpoint(
            &key,
            local_flight,
            local_node,
            None,
            &ads,
            |_replica| async move { None },
        )
        .await;
        assert_eq!(picked, None);
    }

    #[tokio::test]
    async fn gossip_higher_hint_wins_without_replica_confirm() {
        let local_node = NodeId::generate();
        let peer = NodeId::generate();
        let local_flight: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let peer_replica: SocketAddr = "127.0.0.1:2".parse().unwrap();
        let ads = vec![
            ad(
                local_node,
                local_flight,
                "127.0.0.1:1".parse().unwrap(),
                Some(5),
                "p",
            ),
            ad(
                peer,
                "127.0.0.1:10".parse().unwrap(),
                peer_replica,
                Some(99),
                "p",
            ),
        ];
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let picked = pick_wal_endpoint(
            &key,
            local_flight,
            local_node,
            Some(5),
            &ads,
            |replica| async move {
                if replica == peer_replica {
                    None
                } else {
                    Some(5)
                }
            },
        )
        .await;
        assert_eq!(picked, Some("127.0.0.1:10".parse().unwrap()));
    }

    #[tokio::test]
    async fn local_without_paths_falls_through_to_peer() {
        let local_node = NodeId::generate();
        let peer = NodeId::generate();
        let local_flight: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let peer_flight: SocketAddr = "127.0.0.1:10".parse().unwrap();
        let peer_replica: SocketAddr = "127.0.0.1:2".parse().unwrap();
        let ads = vec![
            ad(
                local_node,
                local_flight,
                "127.0.0.1:1".parse().unwrap(),
                Some(9),
                "p",
            ),
            ad(peer, peer_flight, peer_replica, Some(5), "p"),
        ];
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let picked = pick_wal_endpoint(
            &key,
            local_flight,
            local_node,
            None,
            &ads,
            |replica| async move {
                if replica == peer_replica {
                    Some(5)
                } else {
                    None
                }
            },
        )
        .await;
        assert_eq!(picked, Some(peer_flight));
    }

    #[test]
    fn wal_heads_cap_drops_extras() {
        let heads: Vec<WalHeadHint> = (0..40)
            .map(|i| WalHeadHint {
                pipeline: format!("p{i}"),
                committed_index: i,
            })
            .collect();
        assert_eq!(cap_wal_heads(heads).len(), WAL_HEAD_CAP);
    }
}
