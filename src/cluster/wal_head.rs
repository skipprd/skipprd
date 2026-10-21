use std::collections::HashMap;
use std::net::SocketAddr;

use skippr_lease::{NodeId, PipelineKey};

use crate::buffer::wal_handle;
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
    pub catalog_seq_seen: Option<u64>,
    pub payload_ready: bool,
    pub is_local: bool,
}

pub fn cap_wal_heads(mut heads: Vec<WalHeadHint>) -> Vec<WalHeadHint> {
    heads.truncate(WAL_HEAD_CAP);
    heads
}

pub async fn collect_local_wal_heads(registry: Option<&ReplicaRegistry>) -> Vec<WalHeadHint> {
    let mut heads: HashMap<String, (u64, bool)> = HashMap::new();
    for handle in wal_handle::all_wal_handles() {
        let seq = handle.catalog_seq_seen().unwrap_or(0);
        let ready = handle.payload_ready().unwrap_or(false);
        heads.insert(handle.key().pipeline().to_string(), (seq, ready));
    }
    if let Some(registry) = registry {
        for session in registry.snapshot().await {
            let pipeline = session.key.pipeline().to_string();
            let status = session.status().await;
            heads
                .entry(pipeline)
                .and_modify(|(seq, ready)| {
                    *seq = (*seq).max(status.catalog_seq_seen);
                    *ready = *ready || status.payload_ready;
                })
                .or_insert((status.catalog_seq_seen, status.payload_ready));
        }
    }
    let mut out: Vec<WalHeadHint> = heads
        .into_iter()
        .map(
            |(pipeline, (catalog_seq_seen, payload_ready))| WalHeadHint {
                pipeline,
                catalog_seq_seen,
                payload_ready,
            },
        )
        .collect();
    out.sort_by(|a, b| a.pipeline.cmp(&b.pipeline));
    cap_wal_heads(out)
}

pub fn candidates_for_pipeline(
    pipeline: &str,
    local_flight: SocketAddr,
    local_node: NodeId,
    local_seq: Option<u64>,
    ads: &[GossipAd],
) -> Vec<WalCandidate> {
    let mut out: Vec<WalCandidate> = ads
        .iter()
        .filter(|ad| ad.ready)
        .map(|ad| {
            let hint = ad.wal_heads.iter().find(|hint| hint.pipeline == pipeline);
            WalCandidate {
                node_id: ad.node_id,
                flight: ad.flight,
                replica: ad.replica,
                ready: ad.ready,
                catalog_seq_seen: hint.map(|hint| hint.catalog_seq_seen),
                payload_ready: hint.map(|hint| hint.payload_ready).unwrap_or(false),
                is_local: ad.flight == local_flight || ad.node_id == local_node,
            }
        })
        .collect();
    if let Some(seq) = local_seq {
        match out.iter_mut().find(|cand| cand.is_local) {
            Some(local) => {
                local.catalog_seq_seen = Some(local.catalog_seq_seen.unwrap_or(0).max(seq));
                local.payload_ready = true;
            }
            None => out.push(WalCandidate {
                node_id: local_node,
                flight: local_flight,
                replica: local_flight,
                ready: true,
                catalog_seq_seen: Some(seq),
                payload_ready: true,
                is_local: true,
            }),
        }
    }
    out
}

pub fn rank_wal_candidates(mut cands: Vec<WalCandidate>) -> Vec<WalCandidate> {
    cands.retain(|cand| cand.ready && cand.payload_ready && cand.catalog_seq_seen.is_some());
    cands.sort_by(|a, b| {
        b.catalog_seq_seen
            .cmp(&a.catalog_seq_seen)
            .then_with(|| b.is_local.cmp(&a.is_local))
            .then_with(|| a.node_id.to_string().cmp(&b.node_id.to_string()))
    });
    cands
}

pub async fn pick_wal_endpoint<F, Fut>(
    pipeline: &PipelineKey,
    local_flight: SocketAddr,
    local_node: NodeId,
    local_seq: Option<u64>,
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
        local_seq,
        ads,
    );
    if !cands.iter().any(|cand| cand.catalog_seq_seen.is_some()) {
        for cand in cands.iter_mut().filter(|cand| cand.ready && !cand.is_local) {
            if let Some(seq) = confirm(cand.replica).await {
                cand.catalog_seq_seen = Some(seq);
                cand.payload_ready = true;
            }
        }
        if let Some(seq) = local_seq {
            for cand in &mut cands {
                if cand.is_local {
                    cand.catalog_seq_seen = Some(seq);
                    cand.payload_ready = true;
                }
            }
        }
    }
    for cand in rank_wal_candidates(cands) {
        if cand.is_local && local_seq.is_none() {
            continue;
        }
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
        .filter(|status| status.payload_ready)
        .map(|status| status.catalog_seq_seen)
}

pub async fn local_catalog_seq_for(
    pipeline: &PipelineKey,
    registry: Option<&ReplicaRegistry>,
) -> Option<u64> {
    if let Some(handle) = wal_handle::wal_handle_for(pipeline) {
        if handle.payload_ready().ok()? {
            return handle.catalog_seq_seen().ok();
        }
        return None;
    }
    if let Some(registry) = registry {
        if let Some(session) = registry.get(pipeline).await {
            let status = session.status().await;
            if status.payload_ready {
                return Some(status.catalog_seq_seen);
            }
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
        seq: Option<u64>,
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
        if let Some(catalog_seq_seen) = seq {
            ad.wal_heads = vec![WalHeadHint {
                pipeline: pipeline.into(),
                catalog_seq_seen,
                payload_ready: true,
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

    #[test]
    fn wal_heads_cap_drops_extras() {
        let heads: Vec<WalHeadHint> = (0..40)
            .map(|i| WalHeadHint {
                pipeline: format!("p{i}"),
                catalog_seq_seen: i,
                payload_ready: true,
            })
            .collect();
        assert_eq!(cap_wal_heads(heads).len(), WAL_HEAD_CAP);
    }
}
