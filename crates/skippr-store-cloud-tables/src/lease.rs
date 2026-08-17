use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use skippr_lease::{
    LeaseEpoch, LeaseError, LeaseObservation, LeaseSession, NodeId, PipelineKey, PipelineLeaseStore,
};
use skippr_tables_client::{
    attr_bool, attr_n, attr_s, bflag, n, s, TablesClient, TablesClientError,
};
use tracing::warn;
use uuid::Uuid;

pub struct CloudTablesLeaseStore {
    client: Arc<TablesClient>,
    table: String,
}

impl CloudTablesLeaseStore {
    pub fn new(client: Arc<TablesClient>, table: String) -> Self {
        Self { client, table }
    }

    pub async fn connect(table: String) -> Result<Self, LeaseError> {
        if table.is_empty() {
            return Err(LeaseError::StoreUnavailable(
                "SKIPPR_OFFSET_DYNAMODB_TABLE is required for clustered leases".into(),
            ));
        }
        let client = TablesClient::from_env()
            .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
        Ok(Self::new(Arc::new(client), table))
    }

    fn map_conditional<T>(err: TablesClientError) -> Result<T, LeaseError> {
        if err.is_conditional_check_failed() {
            Err(LeaseError::ConditionalRace)
        } else {
            Err(LeaseError::StoreUnavailable(err.to_string()))
        }
    }

    fn decode(item: &serde_json::Value) -> Result<LeaseObservation, LeaseError> {
        let owner = attr_s(item, "owner_node")
            .ok_or_else(|| LeaseError::StoreUnavailable("lease missing owner_node".into()))?;
        let owner = Uuid::parse_str(&owner)
            .map(NodeId::from_uuid)
            .map_err(|err| LeaseError::StoreUnavailable(format!("invalid owner_node: {err}")))?;
        let epoch = attr_n(item, "epoch")
            .ok_or_else(|| LeaseError::StoreUnavailable("lease missing epoch".into()))?;
        let heartbeat = attr_n(item, "heartbeat")
            .ok_or_else(|| LeaseError::StoreUnavailable("lease missing heartbeat".into()))?;
        Ok(LeaseObservation {
            owner,
            epoch: LeaseEpoch::new(epoch),
            heartbeat,
            released: attr_bool(item, "released").unwrap_or(false),
            initialized: attr_bool(item, "initialized").unwrap_or(false),
        })
    }

    async fn takeover(
        &self,
        key: &PipelineKey,
        observed: &LeaseObservation,
        owner: &NodeId,
        require_released: bool,
    ) -> Result<LeaseObservation, LeaseError> {
        let now = chrono::Utc::now().to_rfc3339();
        let values = json!({
            ":old_owner": s(observed.owner.to_string()),
            ":old_epoch": n(observed.epoch.get()),
            ":old_heartbeat": n(observed.heartbeat),
            ":released": bflag(require_released),
            ":new_owner": s(owner.to_string()),
            ":one": n(1),
            ":false": bflag(false),
            ":debug_now": s(now),
        });
        match self
            .client
            .update_item(
                &self.table,
                &key.dynamo_pk(),
                "lease",
                "SET owner_node = :new_owner, epoch = epoch + :one, heartbeat = :one, released = :false, updated_at = :debug_now",
                values,
                Some("owner_node = :old_owner AND epoch = :old_epoch AND heartbeat = :old_heartbeat AND released = :released"),
                true,
            )
            .await
        {
            Ok(Some(item)) => Self::decode(&item),
            Ok(None) => Err(LeaseError::StoreUnavailable(
                "takeover returned no attributes".into(),
            )),
            Err(err) => Self::map_conditional(err),
        }
    }
}

#[async_trait]
impl PipelineLeaseStore for CloudTablesLeaseStore {
    async fn create(
        &self,
        key: &PipelineKey,
        owner: &NodeId,
    ) -> Result<LeaseObservation, LeaseError> {
        let now = chrono::Utc::now().to_rfc3339();
        let item = json!({
            "PK": s(key.dynamo_pk()),
            "SK": s("lease"),
            "owner_node": s(owner.to_string()),
            "epoch": n(1),
            "heartbeat": n(1),
            "released": bflag(false),
            "initialized": bflag(false),
            "updated_at": s(now),
        });
        match self
            .client
            .put_item(&self.table, item, Some("attribute_not_exists(PK)"), None)
            .await
        {
            Ok(()) => Ok(LeaseObservation {
                owner: *owner,
                epoch: LeaseEpoch::new(1),
                heartbeat: 1,
                released: false,
                initialized: false,
            }),
            Err(err) => Self::map_conditional(err),
        }
    }

    async fn read_consistent(
        &self,
        key: &PipelineKey,
    ) -> Result<Option<LeaseObservation>, LeaseError> {
        let item = self
            .client
            .get_item(&self.table, &key.dynamo_pk(), "lease", true)
            .await
            .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?;
        item.as_ref().map(Self::decode).transpose()
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
        let now = chrono::Utc::now().to_rfc3339();
        let values = json!({
            ":owner": s(session.owner.to_string()),
            ":epoch": n(session.epoch.get()),
            ":false": bflag(false),
            ":one": n(1),
            ":debug_now": s(now),
        });
        match self
            .client
            .update_item(
                &self.table,
                &key.dynamo_pk(),
                "lease",
                "SET heartbeat = heartbeat + :one, updated_at = :debug_now",
                values,
                Some("owner_node = :owner AND epoch = :epoch AND released = :false"),
                true,
            )
            .await
        {
            Ok(Some(item)) => Self::decode(&item),
            Ok(None) => Err(LeaseError::StoreUnavailable(
                "renew returned no attributes".into(),
            )),
            Err(err) => {
                let mapped = Self::map_conditional::<LeaseObservation>(err);
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
        let now = chrono::Utc::now().to_rfc3339();
        let values = json!({
            ":owner": s(session.owner.to_string()),
            ":epoch": n(session.epoch.get()),
            ":false": bflag(false),
            ":true": bflag(true),
            ":debug_now": s(now),
        });
        self.client
            .update_item(
                &self.table,
                &key.dynamo_pk(),
                "lease",
                "SET initialized = :true, updated_at = :debug_now",
                values,
                Some("owner_node = :owner AND epoch = :epoch AND released = :false"),
                false,
            )
            .await
            .map(|_| ())
            .map_err(|err| match Self::map_conditional::<()>(err) {
                Err(LeaseError::ConditionalRace) => LeaseError::Lost,
                Err(other) => other,
                Ok(()) => LeaseError::Lost,
            })
    }

    async fn release_after_drain(
        &self,
        key: &PipelineKey,
        session: &LeaseSession,
    ) -> Result<(), LeaseError> {
        let now = chrono::Utc::now().to_rfc3339();
        let values = json!({
            ":owner": s(session.owner.to_string()),
            ":epoch": n(session.epoch.get()),
            ":false": bflag(false),
            ":true": bflag(true),
            ":one": n(1),
            ":debug_now": s(now),
        });
        self.client
            .update_item(
                &self.table,
                &key.dynamo_pk(),
                "lease",
                "SET released = :true, heartbeat = heartbeat + :one, updated_at = :debug_now",
                values,
                Some("owner_node = :owner AND epoch = :epoch AND released = :false"),
                false,
            )
            .await
            .map(|_| ())
            .map_err(|err| {
                warn!("lease release failed: {err}");
                match Self::map_conditional::<()>(err) {
                    Err(LeaseError::ConditionalRace) => LeaseError::Lost,
                    Err(other) => other,
                    Ok(()) => LeaseError::Lost,
                }
            })
    }
}
