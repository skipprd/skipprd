use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client;
use skippr_lease::{
    ClusterId, ClusterMembershipStore, HostId, LeaseError, MembershipRecord, NodeAd, NodeId,
    PROTOCOL_MAX, PROTOCOL_MIN,
};
use uuid::Uuid;

pub struct DynamoDbMembershipStore {
    client: Arc<Client>,
    table: String,
}

impl DynamoDbMembershipStore {
    pub fn new(client: Arc<Client>, table: String) -> Self {
        Self { client, table }
    }

    pub async fn connect(table: String) -> Result<Self, LeaseError> {
        if table.is_empty() {
            return Err(LeaseError::StoreUnavailable(
                "SKIPPR_OFFSET_DYNAMODB_TABLE is required for clustered membership".into(),
            ));
        }
        let shared = crate::load_sdk_config().await;
        Ok(Self::new(Arc::new(Client::new(&shared)), table))
    }

    fn node_sk(node: &NodeId) -> String {
        format!("node#{node}")
    }

    pub async fn put(&self, cluster: &ClusterId, ad: &NodeAd) -> Result<(), LeaseError> {
        let now = chrono::Utc::now().to_rfc3339();
        self.client
            .put_item()
            .table_name(&self.table)
            .item("PK", AttributeValue::S(cluster.membership_pk()))
            .item("SK", AttributeValue::S(Self::node_sk(&ad.node_id)))
            .item("host_label", AttributeValue::S(ad.host_label.clone()))
            .item(
                "host_id",
                AttributeValue::S(ad.host_id.as_str().to_string()),
            )
            .item(
                "replica_addr",
                AttributeValue::S(ad.replica_addr.to_string()),
            )
            .item("flight_addr", AttributeValue::S(ad.flight_addr.to_string()))
            .item("gossip_addr", AttributeValue::S(ad.gossip_addr.to_string()))
            .item("heartbeat", AttributeValue::N(ad.heartbeat.to_string()))
            .item(
                "protocol_min",
                AttributeValue::N(ad.protocol_min.to_string()),
            )
            .item(
                "protocol_max",
                AttributeValue::N(ad.protocol_max.to_string()),
            )
            .item("capabilities", AttributeValue::S(ad.capabilities.clone()))
            .item("ready", AttributeValue::Bool(ad.ready))
            .item("disk_pressure", AttributeValue::Bool(ad.disk_pressure))
            .item("updated_at", AttributeValue::S(now))
            .send()
            .await
            .map(|_| ())
            .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))
    }

    pub async fn query_cluster(
        &self,
        cluster: &ClusterId,
    ) -> Result<Vec<MembershipRecord>, LeaseError> {
        let out = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression("PK = :pk AND begins_with(SK, :sk)")
            .expression_attribute_values(":pk", AttributeValue::S(cluster.membership_pk()))
            .expression_attribute_values(":sk", AttributeValue::S("node#".into()))
            .send()
            .await
            .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
        let mut records = Vec::new();
        for item in out.items() {
            if let Ok(record) = decode_membership(item) {
                records.push(record);
            }
        }
        Ok(records)
    }

    pub async fn delete_generation(
        &self,
        cluster: &ClusterId,
        node: &NodeId,
    ) -> Result<(), LeaseError> {
        self.delete_if_heartbeat(cluster, node, None).await
    }

    pub async fn delete_if_heartbeat(
        &self,
        cluster: &ClusterId,
        node: &NodeId,
        heartbeat: Option<u64>,
    ) -> Result<(), LeaseError> {
        let mut del = self
            .client
            .delete_item()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(cluster.membership_pk()))
            .key("SK", AttributeValue::S(Self::node_sk(node)));
        if let Some(heartbeat) = heartbeat {
            del = del
                .condition_expression("heartbeat = :h")
                .expression_attribute_values(":h", AttributeValue::N(heartbeat.to_string()));
        }
        match del.send().await {
            Ok(_) => Ok(()),
            Err(err) if crate::is_conditional_check_failed(&err) => Ok(()),
            Err(err) => Err(LeaseError::StoreUnavailable(err.to_string())),
        }
    }
}

#[async_trait::async_trait]
impl ClusterMembershipStore for DynamoDbMembershipStore {
    async fn put(&self, cluster: &ClusterId, ad: &NodeAd) -> Result<(), LeaseError> {
        DynamoDbMembershipStore::put(self, cluster, ad).await
    }

    async fn query_cluster(
        &self,
        cluster: &ClusterId,
    ) -> Result<Vec<MembershipRecord>, LeaseError> {
        DynamoDbMembershipStore::query_cluster(self, cluster).await
    }

    async fn delete_if_heartbeat(
        &self,
        cluster: &ClusterId,
        node: &NodeId,
        heartbeat: Option<u64>,
    ) -> Result<(), LeaseError> {
        DynamoDbMembershipStore::delete_if_heartbeat(self, cluster, node, heartbeat).await
    }
}

fn decode_membership(
    item: &HashMap<String, AttributeValue>,
) -> Result<MembershipRecord, LeaseError> {
    let sk = item
        .get("SK")
        .and_then(|v| v.as_s().ok())
        .ok_or_else(|| LeaseError::StoreUnavailable("membership missing SK".into()))?;
    let uuid = sk
        .strip_prefix("node#")
        .ok_or_else(|| LeaseError::StoreUnavailable("membership SK is not node#".into()))?;
    let node_id = Uuid::from_str(uuid)
        .map(NodeId::from_uuid)
        .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
    let host_id = HostId::new(
        item.get("host_id")
            .and_then(|v| v.as_s().ok())
            .cloned()
            .unwrap_or_default(),
    )
    .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
    let parse_addr = |name: &str| -> Result<std::net::SocketAddr, LeaseError> {
        item.get(name)
            .and_then(|v| v.as_s().ok())
            .ok_or_else(|| LeaseError::StoreUnavailable(format!("membership missing {name}")))?
            .parse()
            .map_err(|err| LeaseError::StoreUnavailable(format!("invalid {name}: {err}")))
    };
    Ok(MembershipRecord {
        ad: NodeAd {
            node_id,
            host_id,
            host_label: item
                .get("host_label")
                .and_then(|v| v.as_s().ok())
                .cloned()
                .unwrap_or_default(),
            replica_addr: parse_addr("replica_addr")?,
            flight_addr: parse_addr("flight_addr")?,
            gossip_addr: parse_addr("gossip_addr")?,
            heartbeat: item
                .get("heartbeat")
                .and_then(|v| v.as_n().ok())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
            protocol_min: item
                .get("protocol_min")
                .and_then(|v| v.as_n().ok())
                .and_then(|n| n.parse().ok())
                .unwrap_or(PROTOCOL_MIN),
            protocol_max: item
                .get("protocol_max")
                .and_then(|v| v.as_n().ok())
                .and_then(|n| n.parse().ok())
                .unwrap_or(PROTOCOL_MAX),
            capabilities: item
                .get("capabilities")
                .and_then(|v| v.as_s().ok())
                .cloned()
                .unwrap_or_default(),
            ready: item
                .get("ready")
                .and_then(|v| v.as_bool().ok())
                .copied()
                .unwrap_or(false),
            disk_pressure: item
                .get("disk_pressure")
                .and_then(|v| v.as_bool().ok())
                .copied()
                .unwrap_or(false),
        },
    })
}
