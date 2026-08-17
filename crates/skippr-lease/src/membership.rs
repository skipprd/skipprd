use crate::error::LeaseError;
use crate::identity::{ClusterId, HostId, NodeId};
use crate::{PROTOCOL_MAX, PROTOCOL_MIN};
use async_trait::async_trait;
use std::net::SocketAddr;

/// Cold-discovery membership advertisement (tables / Dynamo PK `cluster`, SK `node#`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeAd {
    pub node_id: NodeId,
    pub host_id: HostId,
    pub host_label: String,
    pub replica_addr: SocketAddr,
    pub flight_addr: SocketAddr,
    pub gossip_addr: SocketAddr,
    pub heartbeat: u64,
    pub protocol_min: u32,
    pub protocol_max: u32,
    pub capabilities: String,
    pub ready: bool,
    pub disk_pressure: bool,
}

impl NodeAd {
    pub fn protocol_defaults() -> (u32, u32) {
        (PROTOCOL_MIN, PROTOCOL_MAX)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MembershipRecord {
    pub ad: NodeAd,
}

#[async_trait]
pub trait ClusterMembershipStore: Send + Sync {
    async fn put(&self, cluster: &ClusterId, ad: &NodeAd) -> Result<(), LeaseError>;
    async fn query_cluster(&self, cluster: &ClusterId)
        -> Result<Vec<MembershipRecord>, LeaseError>;
    async fn delete_if_heartbeat(
        &self,
        cluster: &ClusterId,
        node: &NodeId,
        heartbeat: Option<u64>,
    ) -> Result<(), LeaseError>;
}
