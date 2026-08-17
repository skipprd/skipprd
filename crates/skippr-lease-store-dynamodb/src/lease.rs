use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use aws_sdk_dynamodb::types::{AttributeValue, ReturnValue};
use aws_sdk_dynamodb::Client;
use skippr_lease::{
    LeaseEpoch, LeaseError, LeaseObservation, LeaseSession, NodeId, PipelineKey, PipelineLeaseStore,
};
use tracing::warn;
use uuid::Uuid;

pub struct DynamoDbLeaseStore {
    client: Arc<Client>,
    table: String,
}

impl DynamoDbLeaseStore {
    pub fn new(client: Arc<Client>, table: String) -> Self {
        Self { client, table }
    }

    pub async fn connect(table: String) -> Result<Self, LeaseError> {
        if table.is_empty() {
            return Err(LeaseError::StoreUnavailable(
                "SKIPPR_OFFSET_DYNAMODB_TABLE is required for clustered leases".into(),
            ));
        }
        let shared = crate::load_sdk_config().await;
        Ok(Self::new(Arc::new(Client::new(&shared)), table))
    }

    fn keys(key: &PipelineKey) -> (AttributeValue, AttributeValue) {
        (
            AttributeValue::S(key.dynamo_pk()),
            AttributeValue::S("lease".to_string()),
        )
    }

    fn decode(item: &HashMap<String, AttributeValue>) -> Result<LeaseObservation, LeaseError> {
        let owner = item
            .get("owner_node")
            .and_then(|v| v.as_s().ok())
            .ok_or_else(|| LeaseError::StoreUnavailable("lease missing owner_node".into()))?;
        let owner = Uuid::from_str(owner)
            .map(NodeId::from_uuid)
            .map_err(|err| LeaseError::StoreUnavailable(format!("invalid owner_node: {err}")))?;
        let epoch = item
            .get("epoch")
            .and_then(|v| v.as_n().ok())
            .and_then(|n| n.parse::<u64>().ok())
            .ok_or_else(|| LeaseError::StoreUnavailable("lease missing epoch".into()))?;
        let heartbeat = item
            .get("heartbeat")
            .and_then(|v| v.as_n().ok())
            .and_then(|n| n.parse::<u64>().ok())
            .ok_or_else(|| LeaseError::StoreUnavailable("lease missing heartbeat".into()))?;
        let released = item
            .get("released")
            .and_then(|v| v.as_bool().ok())
            .copied()
            .unwrap_or(false);
        let initialized = item
            .get("initialized")
            .and_then(|v| v.as_bool().ok())
            .copied()
            .unwrap_or(false);
        Ok(LeaseObservation {
            owner,
            epoch: LeaseEpoch::new(epoch),
            heartbeat,
            released,
            initialized,
        })
    }

    fn map_conditional<T>(err: aws_sdk_dynamodb::Error) -> Result<T, LeaseError> {
        if crate::is_conditional_check_failed(&err) {
            Err(LeaseError::ConditionalRace)
        } else {
            Err(LeaseError::StoreUnavailable(err.to_string()))
        }
    }
}

#[async_trait]
impl PipelineLeaseStore for DynamoDbLeaseStore {
    async fn create(
        &self,
        key: &PipelineKey,
        owner: &NodeId,
    ) -> Result<LeaseObservation, LeaseError> {
        let (pk, sk) = Self::keys(key);
        let now = chrono::Utc::now().to_rfc3339();
        let result = self
            .client
            .put_item()
            .table_name(&self.table)
            .item("PK", pk)
            .item("SK", sk)
            .item("owner_node", AttributeValue::S(owner.to_string()))
            .item("epoch", AttributeValue::N("1".into()))
            .item("heartbeat", AttributeValue::N("1".into()))
            .item("released", AttributeValue::Bool(false))
            .item("initialized", AttributeValue::Bool(false))
            .item("updated_at", AttributeValue::S(now))
            .condition_expression("attribute_not_exists(PK)")
            .send()
            .await;
        match result {
            Ok(_) => Ok(LeaseObservation {
                owner: *owner,
                epoch: LeaseEpoch::new(1),
                heartbeat: 1,
                released: false,
                initialized: false,
            }),
            Err(err) => Self::map_conditional(err.into()),
        }
    }

    async fn read_consistent(
        &self,
        key: &PipelineKey,
    ) -> Result<Option<LeaseObservation>, LeaseError> {
        let (pk, sk) = Self::keys(key);
        let result = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", pk)
            .key("SK", sk)
            .consistent_read(true)
            .send()
            .await
            .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
        result.item.as_ref().map(Self::decode).transpose()
    }

    async fn acquire_released(
        &self,
        key: &PipelineKey,
        observed: &LeaseObservation,
        owner: &NodeId,
    ) -> Result<LeaseObservation, LeaseError> {
        self.takeover(key, observed, owner, true).await
    }

    async fn steal_unchanged(
        &self,
        key: &PipelineKey,
        observed: &LeaseObservation,
        owner: &NodeId,
    ) -> Result<LeaseObservation, LeaseError> {
        self.takeover(key, observed, owner, false).await
    }

    async fn renew(
        &self,
        key: &PipelineKey,
        session: &LeaseSession,
    ) -> Result<LeaseObservation, LeaseError> {
        let (pk, sk) = Self::keys(key);
        let now = chrono::Utc::now().to_rfc3339();
        let result = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", pk)
            .key("SK", sk)
            .condition_expression("owner_node = :owner AND epoch = :epoch AND released = :false")
            .update_expression("SET heartbeat = heartbeat + :one, updated_at = :debug_now")
            .expression_attribute_values(":owner", AttributeValue::S(session.owner.to_string()))
            .expression_attribute_values(
                ":epoch",
                AttributeValue::N(session.epoch.get().to_string()),
            )
            .expression_attribute_values(":false", AttributeValue::Bool(false))
            .expression_attribute_values(":one", AttributeValue::N("1".into()))
            .expression_attribute_values(":debug_now", AttributeValue::S(now))
            .return_values(ReturnValue::AllNew)
            .send()
            .await;
        match result {
            Ok(out) => {
                let item = out.attributes.ok_or_else(|| {
                    LeaseError::StoreUnavailable("renew returned no attributes".into())
                })?;
                Self::decode(&item)
            }
            Err(err) => {
                let mapped = Self::map_conditional::<LeaseObservation>(err.into());
                if matches!(mapped, Err(LeaseError::ConditionalRace)) {
                    Err(LeaseError::Lost)
                } else {
                    mapped
                }
            }
        }
    }

    async fn mark_initialized(
        &self,
        key: &PipelineKey,
        session: &LeaseSession,
    ) -> Result<(), LeaseError> {
        let (pk, sk) = Self::keys(key);
        let now = chrono::Utc::now().to_rfc3339();
        self.client
            .update_item()
            .table_name(&self.table)
            .key("PK", pk)
            .key("SK", sk)
            .condition_expression("owner_node = :owner AND epoch = :epoch AND released = :false")
            .update_expression("SET initialized = :true, updated_at = :debug_now")
            .expression_attribute_values(":owner", AttributeValue::S(session.owner.to_string()))
            .expression_attribute_values(
                ":epoch",
                AttributeValue::N(session.epoch.get().to_string()),
            )
            .expression_attribute_values(":false", AttributeValue::Bool(false))
            .expression_attribute_values(":true", AttributeValue::Bool(true))
            .expression_attribute_values(":debug_now", AttributeValue::S(now))
            .send()
            .await
            .map(|_| ())
            .map_err(|err| {
                let mapped = Self::map_conditional::<()>(err.into());
                match mapped {
                    Err(LeaseError::ConditionalRace) => LeaseError::Lost,
                    Err(other) => other,
                    Ok(()) => LeaseError::Lost,
                }
            })
    }

    async fn release_after_drain(
        &self,
        key: &PipelineKey,
        session: &LeaseSession,
    ) -> Result<(), LeaseError> {
        let (pk, sk) = Self::keys(key);
        let now = chrono::Utc::now().to_rfc3339();
        self.client
            .update_item()
            .table_name(&self.table)
            .key("PK", pk)
            .key("SK", sk)
            .condition_expression("owner_node = :owner AND epoch = :epoch AND released = :false")
            .update_expression(
                "SET released = :true, heartbeat = heartbeat + :one, updated_at = :debug_now",
            )
            .expression_attribute_values(":owner", AttributeValue::S(session.owner.to_string()))
            .expression_attribute_values(
                ":epoch",
                AttributeValue::N(session.epoch.get().to_string()),
            )
            .expression_attribute_values(":false", AttributeValue::Bool(false))
            .expression_attribute_values(":true", AttributeValue::Bool(true))
            .expression_attribute_values(":one", AttributeValue::N("1".into()))
            .expression_attribute_values(":debug_now", AttributeValue::S(now))
            .send()
            .await
            .map(|_| ())
            .map_err(|err| {
                warn!("lease release failed: {err}");
                let mapped = Self::map_conditional::<()>(err.into());
                match mapped {
                    Err(LeaseError::ConditionalRace) => LeaseError::Lost,
                    Err(other) => other,
                    Ok(()) => LeaseError::Lost,
                }
            })
    }
}

impl DynamoDbLeaseStore {
    async fn takeover(
        &self,
        key: &PipelineKey,
        observed: &LeaseObservation,
        owner: &NodeId,
        require_released: bool,
    ) -> Result<LeaseObservation, LeaseError> {
        let (pk, sk) = Self::keys(key);
        let now = chrono::Utc::now().to_rfc3339();
        let released_cond = if require_released { ":true" } else { ":false" };
        let result = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", pk)
            .key("SK", sk)
            .condition_expression(
                "owner_node = :old_owner AND epoch = :old_epoch AND heartbeat = :old_heartbeat AND released = :released",
            )
            .update_expression(
                "SET owner_node = :new_owner, epoch = epoch + :one, heartbeat = :one, released = :false, updated_at = :debug_now",
            )
            .expression_attribute_values(":old_owner", AttributeValue::S(observed.owner.to_string()))
            .expression_attribute_values(":old_epoch", AttributeValue::N(observed.epoch.get().to_string()))
            .expression_attribute_values(
                ":old_heartbeat",
                AttributeValue::N(observed.heartbeat.to_string()),
            )
            .expression_attribute_values(":released", AttributeValue::Bool(require_released))
            .expression_attribute_values(":new_owner", AttributeValue::S(owner.to_string()))
            .expression_attribute_values(":one", AttributeValue::N("1".into()))
            .expression_attribute_values(":false", AttributeValue::Bool(false))
            .expression_attribute_values(":debug_now", AttributeValue::S(now))
            .return_values(ReturnValue::AllNew)
            .send()
            .await;
        let _ = released_cond;
        match result {
            Ok(out) => {
                let item = out.attributes.ok_or_else(|| {
                    LeaseError::StoreUnavailable("takeover returned no attributes".into())
                })?;
                Self::decode(&item)
            }
            Err(err) => Self::map_conditional(err.into()),
        }
    }
}
