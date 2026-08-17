use sha2::{Digest, Sha256};
use skippr_lease::{HostId, NodeId, PipelineKey};

#[derive(Clone, Debug)]
pub struct ReplicaCandidate {
    pub node_id: NodeId,
    pub host_id: HostId,
    pub ready: bool,
    pub protocol_min: u32,
    pub protocol_max: u32,
    pub endpoint: std::net::SocketAddr,
    pub committed_lag: u64,
    pub disk_pressure: bool,
}

pub fn replica_rank(key: &PipelineKey, node: &NodeId) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(key.canonical_bytes());
    hasher.update(node.as_bytes());
    hasher.finalize().into()
}

pub fn rank_replicas(
    key: &PipelineKey,
    self_id: NodeId,
    self_host: &HostId,
    mut candidates: Vec<ReplicaCandidate>,
) -> Vec<ReplicaCandidate> {
    candidates.retain(|c| {
        c.node_id != self_id
            && &c.host_id != self_host
            && !c.disk_pressure
            && crate::cluster::peer::protocol_compatible(c.protocol_min, c.protocol_max)
    });
    candidates.sort_by(|a, b| {
        a.committed_lag
            .cmp(&b.committed_lag)
            .then_with(|| replica_rank(key, &b.node_id).cmp(&replica_rank(key, &a.node_id)))
    });
    candidates
}

pub fn select_replica(
    key: &PipelineKey,
    self_id: NodeId,
    self_host: &HostId,
    candidates: Vec<ReplicaCandidate>,
) -> Option<ReplicaCandidate> {
    rank_replicas(key, self_id, self_host, candidates)
        .into_iter()
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::PipelineKey;

    #[test]
    fn same_host_is_never_selected() {
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let self_id = NodeId::generate();
        let host = HostId::new("host-a").unwrap();
        let other = NodeId::generate();
        let selected = select_replica(
            &key,
            self_id,
            &host,
            vec![ReplicaCandidate {
                node_id: other,
                host_id: host.clone(),
                ready: true,
                protocol_min: skippr_lease::PROTOCOL_MIN,
                protocol_max: skippr_lease::PROTOCOL_MAX,
                endpoint: "127.0.0.1:9".parse().unwrap(),
                committed_lag: 0,
                disk_pressure: false,
            }],
        );
        assert!(selected.is_none());
    }

    #[test]
    fn rendezvous_hash_is_stable() {
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let node = NodeId::generate();
        assert_eq!(replica_rank(&key, &node), replica_rank(&key, &node));
    }

    #[test]
    fn disk_pressure_is_never_selected() {
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let self_id = NodeId::generate();
        let host = HostId::new("host-a").unwrap();
        let other = NodeId::generate();
        let selected = select_replica(
            &key,
            self_id,
            &host,
            vec![ReplicaCandidate {
                node_id: other,
                host_id: HostId::new("host-b").unwrap(),
                ready: true,
                protocol_min: skippr_lease::PROTOCOL_MIN,
                protocol_max: skippr_lease::PROTOCOL_MAX,
                endpoint: "127.0.0.1:9".parse().unwrap(),
                committed_lag: 0,
                disk_pressure: true,
            }],
        );
        assert!(selected.is_none());
    }

    #[test]
    fn gossip_ready_is_not_required_for_assign() {
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let self_id = NodeId::generate();
        let host = HostId::new("host-a").unwrap();
        let other = NodeId::generate();
        let selected = select_replica(
            &key,
            self_id,
            &host,
            vec![ReplicaCandidate {
                node_id: other,
                host_id: HostId::new("host-b").unwrap(),
                ready: false,
                protocol_min: skippr_lease::PROTOCOL_MIN,
                protocol_max: skippr_lease::PROTOCOL_MAX,
                endpoint: "127.0.0.1:9".parse().unwrap(),
                committed_lag: 0,
                disk_pressure: false,
            }],
        );
        assert!(selected.is_some());
    }

    #[test]
    fn rank_replicas_returns_all_eligible_hosts() {
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let self_id = NodeId::generate();
        let host = HostId::new("host-a").unwrap();
        let a = NodeId::generate();
        let b = NodeId::generate();
        let ranked = rank_replicas(
            &key,
            self_id,
            &host,
            vec![
                ReplicaCandidate {
                    node_id: a,
                    host_id: HostId::new("host-b").unwrap(),
                    ready: true,
                    protocol_min: skippr_lease::PROTOCOL_MIN,
                    protocol_max: skippr_lease::PROTOCOL_MAX,
                    endpoint: "127.0.0.1:9".parse().unwrap(),
                    committed_lag: 0,
                    disk_pressure: false,
                },
                ReplicaCandidate {
                    node_id: b,
                    host_id: HostId::new("host-c").unwrap(),
                    ready: true,
                    protocol_min: skippr_lease::PROTOCOL_MIN,
                    protocol_max: skippr_lease::PROTOCOL_MAX,
                    endpoint: "127.0.0.1:10".parse().unwrap(),
                    committed_lag: 0,
                    disk_pressure: false,
                },
            ],
        );
        assert_eq!(ranked.len(), 2);
    }
}
