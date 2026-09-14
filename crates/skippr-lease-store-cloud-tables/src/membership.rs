use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use skippr_cloud::{attr_bool, attr_n, attr_s, bflag, n, s, Client};
use skippr_lease::{
    ClusterId, ClusterMembershipStore, HostId, LeaseError, MembershipRecord, NodeAd, NodeId,
    PROTOCOL_MAX, PROTOCOL_MIN,
};
use uuid::Uuid;

pub struct CloudTablesMembershipStore {
    client: Arc<Client>,
    table: String,
}

impl CloudTablesMembershipStore {
    pub fn new(client: Arc<Client>, table: String) -> Self {
        Self { client, table }
    }

    pub async fn connect(table: String) -> Result<Self, LeaseError> {
        if table.is_empty() {
            return Err(LeaseError::StoreUnavailable(
                "SKIPPR_OFFSET_DYNAMODB_TABLE is required for clustered membership".into(),
            ));
        }
        let client =
            Client::from_env().map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
        Ok(Self::new(Arc::new(client), table))
    }

    fn node_sk(node: &NodeId) -> String {
        format!("node#{node}")
    }
}

#[async_trait]
impl ClusterMembershipStore for CloudTablesMembershipStore {
    async fn put(&self, cluster: &ClusterId, ad: &NodeAd) -> Result<(), LeaseError> {
        let now = chrono::Utc::now().to_rfc3339();
        let item = json!({
            "PK": s(cluster.membership_pk()),
            "SK": s(Self::node_sk(&ad.node_id)),
            "host_label": s(ad.host_label.clone()),
            "host_id": s(ad.host_id.as_str().to_string()),
            "replica_addr": s(ad.replica_addr.to_string()),
            "flight_addr": s(ad.flight_addr.to_string()),
            "gossip_addr": s(ad.gossip_addr.to_string()),
            "heartbeat": n(ad.heartbeat),
            "protocol_min": n(ad.protocol_min),
            "protocol_max": n(ad.protocol_max),
            "capabilities": s(ad.capabilities.clone()),
            "ready": bflag(ad.ready),
            "disk_pressure": bflag(ad.disk_pressure),
            "updated_at": s(now),
        });
        self.client
            .put_item(&self.table, item, None, None)
            .await
            .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))
    }

    async fn query_cluster(
        &self,
        cluster: &ClusterId,
    ) -> Result<Vec<MembershipRecord>, LeaseError> {
        let items = self
            .client
            .query(
                &self.table,
                "PK = :pk AND begins_with(SK, :sk)",
                json!({
                    ":pk": s(cluster.membership_pk()),
                    ":sk": s("node#"),
                }),
            )
            .await
            .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
        Ok(items
            .iter()
            .filter_map(|item| decode_membership(item).ok())
            .collect())
    }

    async fn delete_if_heartbeat(
        &self,
        cluster: &ClusterId,
        node: &NodeId,
        heartbeat: Option<u64>,
    ) -> Result<(), LeaseError> {
        let condition = heartbeat.map(|_| "heartbeat = :h");
        let values = heartbeat.map(|h| json!({ ":h": n(h) }));
        match self
            .client
            .delete_item(
                &self.table,
                &cluster.membership_pk(),
                &Self::node_sk(node),
                condition,
                values,
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(err) if err.is_conditional_check_failed() => Ok(()),
            Err(err) => Err(LeaseError::StoreUnavailable(err.to_string())),
        }
    }
}

fn decode_membership(item: &serde_json::Value) -> Result<MembershipRecord, LeaseError> {
    let sk = attr_s(item, "SK")
        .ok_or_else(|| LeaseError::StoreUnavailable("membership missing SK".into()))?;
    let uuid = sk
        .strip_prefix("node#")
        .ok_or_else(|| LeaseError::StoreUnavailable("membership SK is not node#".into()))?;
    let node_id = Uuid::from_str(uuid)
        .map(NodeId::from_uuid)
        .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
    let host_id = HostId::new(attr_s(item, "host_id").unwrap_or_default())
        .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
    let parse_addr = |name: &str| -> Result<std::net::SocketAddr, LeaseError> {
        attr_s(item, name)
            .ok_or_else(|| LeaseError::StoreUnavailable(format!("membership missing {name}")))?
            .parse()
            .map_err(|err| LeaseError::StoreUnavailable(format!("invalid {name}: {err}")))
    };
    Ok(MembershipRecord {
        ad: NodeAd {
            node_id,
            host_id,
            host_label: attr_s(item, "host_label").unwrap_or_default(),
            replica_addr: parse_addr("replica_addr")?,
            flight_addr: parse_addr("flight_addr")?,
            gossip_addr: parse_addr("gossip_addr")?,
            heartbeat: attr_n(item, "heartbeat").unwrap_or(0),
            protocol_min: attr_n(item, "protocol_min")
                .map(|n| n as u32)
                .unwrap_or(PROTOCOL_MIN),
            protocol_max: attr_n(item, "protocol_max")
                .map(|n| n as u32)
                .unwrap_or(PROTOCOL_MAX),
            capabilities: attr_s(item, "capabilities").unwrap_or_default(),
            ready: attr_bool(item, "ready").unwrap_or(false),
            disk_pressure: attr_bool(item, "disk_pressure").unwrap_or(false),
        },
    })
}
